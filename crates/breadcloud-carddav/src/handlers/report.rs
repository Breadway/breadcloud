use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{AddressbookStore, DavPath, StorageError};
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::{read_body, xml_response};
use crate::path::CardPath;
use crate::state::AppState;
use crate::vcard;
use crate::xml::request::{parse_report_body, PropFilter, PropSet, ReportRequest};
use crate::xml::response::{self, ContactEntry, SyncEntry};

fn multiget_path(href: &str) -> Option<String> {
    let uri = href.parse::<Uri>().ok()?;
    let path = percent_encoding::percent_decode_str(uri.path())
        .decode_utf8()
        .ok()?;
    Some(path.into_owned())
}

fn contact_href(user: &str, addressbook_id: &str, obj: &str) -> String {
    format!("/remote.php/dav/addressbooks/{user}/{addressbook_id}/{obj}")
}

/// One sync-collection diff entry built up before rendering -- identical
/// shape to `breadcloud_caldav::handlers::report::SyncBuiltEntry`.
type SyncBuiltEntry = (String, bool, Option<String>, Option<Vec<u8>>);

/// RFC 6578 asks clients with an unrecognized or out-of-range token to
/// retry with an initial sync, marked by the DAV:valid-sync-token condition.
fn invalid_sync_token() -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:error xmlns:d=\"DAV:\"><d:valid-sync-token/></d:error>",
        ))
        .unwrap()
}

/// `REPORT` on an addressbook collection: dispatches to whichever of the
/// three shapes real clients send (RFC 6352 `addressbook-query`/
/// `addressbook-multiget`, RFC 6578 `sync-collection`) -- same three-way
/// split `breadcloud_caldav::handlers::report::handle` does for
/// calendars.
pub async fn handle(state: &AppState, user: &str, addressbook_id: &str, req: Request) -> Response {
    if !state
        .addressbooks
        .addressbook_exists(user, addressbook_id)
        .await
    {
        return (StatusCode::NOT_FOUND, "no such addressbook").into_response();
    }

    let body = read_body(req).await;
    match parse_report_body(&body) {
        ReportRequest::AddressbookMultiget { hrefs, props } => {
            addressbook_multiget(state, user, addressbook_id, &hrefs, &props).await
        }
        ReportRequest::AddressbookQuery { filter, props } => {
            addressbook_query(state, user, addressbook_id, filter.as_ref(), &props).await
        }
        ReportRequest::SyncCollection { sync_token, props } => {
            sync_collection(state, user, addressbook_id, sync_token.as_deref(), &props).await
        }
        ReportRequest::Unsupported => {
            (StatusCode::BAD_REQUEST, "unsupported REPORT type").into_response()
        }
    }
}

/// RFC 6352 8.7: fetch an explicit, known set of contact objects by href.
/// Absolute hrefs and percent-encoded path segments are normalized
/// before parsing. Missing or out-of-scope hrefs receive a 404 response.
async fn addressbook_multiget(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    hrefs: &[String],
    props: &PropSet,
) -> Response {
    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };

    let mut fetched: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut missing = Vec::new();
    for href in hrefs {
        let Some(path) = multiget_path(href) else {
            continue;
        };
        let Some(CardPath::Contact(href_user, href_book, obj)) = CardPath::parse(&path) else {
            missing.push(path);
            continue;
        };
        if href_user != user || href_book != addressbook_id {
            missing.push(path);
            continue;
        }
        let Ok(dav) = DavPath::parse(&format!("/{obj}")) else {
            missing.push(path);
            continue;
        };
        let bytes = match contacts.read_bytes(&dav).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound) => {
                missing.push(path);
                continue;
            }
            Err(err) => return storage_error_response(err),
        };
        let etag = match contacts.etag(&dav).await {
            Ok(etag) => etag,
            Err(StorageError::NotFound) => {
                missing.push(path);
                continue;
            }
            Err(err) => return storage_error_response(err),
        };
        fetched.push((contact_href(user, addressbook_id, &obj), etag, bytes));
    }

    let entries: Vec<ContactEntry> = fetched
        .iter()
        .map(|(href, etag, vcf)| ContactEntry {
            href: href.clone(),
            etag: etag.as_str(),
            vcf: if props.address_data {
                Some(vcf.as_slice())
            } else {
                None
            },
        })
        .collect();
    xml_response(response::addressbook_object_multistatus_with_missing(
        &entries, &missing, props,
    ))
}

/// RFC 6352 8.6: fetch every contact matching an optional property
/// text-match filter. No index -- a linear scan of the addressbook's
/// contacts, fine at this deployment's real scale (76 contacts total;
/// see docs/DESIGN.md).
async fn addressbook_query(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    filter: Option<&PropFilter>,
    props: &PropSet,
) -> Response {
    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };
    let listing = match contacts.list_dir(&DavPath::root()).await {
        Ok(l) => l,
        Err(err) => return storage_error_response(err),
    };

    let mut matched: Vec<(String, String, Vec<u8>)> = Vec::new();
    for entry in listing {
        if entry.metadata.is_dir() {
            continue;
        }
        let Ok(bytes) = contacts.read_bytes(&entry.path).await else {
            continue;
        };
        if let Some(f) = filter {
            if !vcard::text_match(&bytes, &f.name, &f.text) {
                continue;
            }
        }
        let Ok(etag) = contacts.etag_for(&entry.path, &entry.metadata).await else {
            continue;
        };
        let obj = entry.path.name().unwrap_or("").to_string();
        matched.push((contact_href(user, addressbook_id, &obj), etag, bytes));
    }

    let entries: Vec<ContactEntry> = matched
        .iter()
        .map(|(href, etag, vcf)| ContactEntry {
            href: href.clone(),
            etag: etag.as_str(),
            vcf: if props.address_data {
                Some(vcf.as_slice())
            } else {
                None
            },
        })
        .collect();
    xml_response(response::addressbook_object_multistatus(&entries, props))
}

/// RFC 6578: incremental sync, identical semantics (and identical
/// tombstone-dropping-on-initial-sync caveat) to
/// `breadcloud_caldav::handlers::report::sync_collection`.
async fn sync_collection(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    sync_token: Option<&str>,
    props: &PropSet,
) -> Response {
    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };

    let since = match sync_token {
        Some(token) => match AddressbookStore::parse_sync_token(token) {
            Some(since) => Some(since),
            None => return invalid_sync_token(),
        },
        None => None,
    };
    let (changes, new_ctag) = match state
        .addressbooks
        .changes_since(user, addressbook_id, since)
        .await
    {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };
    if since.is_some_and(|since| new_ctag.parse::<u64>().is_ok_and(|current| since > current)) {
        return invalid_sync_token();
    }

    let initial = sync_token.is_none();

    let mut built: Vec<SyncBuiltEntry> = Vec::new();
    for change in changes {
        if initial && change.deleted {
            continue;
        }
        let href = contact_href(user, addressbook_id, &change.uid);
        if change.deleted {
            built.push((href, true, None, None));
            continue;
        }
        let Ok(dav) = DavPath::parse(&format!("/{}", change.uid)) else {
            continue;
        };
        match contacts.etag(&dav).await {
            Ok(etag) => {
                let vcf = if props.address_data {
                    contacts.read_bytes(&dav).await.ok()
                } else {
                    None
                };
                built.push((href, false, Some(etag), vcf));
            }
            Err(_) => built.push((href, true, None, None)),
        }
    }

    let entries: Vec<SyncEntry> = built
        .iter()
        .map(|(href, deleted, etag, vcf)| SyncEntry {
            href: href.clone(),
            deleted: *deleted,
            etag: etag.as_deref(),
            vcf: vcf.as_deref(),
        })
        .collect();

    let new_token = AddressbookStore::format_sync_token(&new_ctag);
    xml_response(response::sync_collection_multistatus(
        &entries, props, &new_token,
    ))
}
