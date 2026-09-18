use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError};
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::{parse_depth, read_propfind_mode, xml_response, Depth};
use crate::state::AppState;
use crate::xml::response::{self, AddressbookEntry};

/// PROPFIND on `/remote.php/dav/addressbooks/<user>/`, the
/// addressbook-home-set collection itself -- identical Depth 0/1 shape to
/// `breadcloud_caldav::handlers::propfind::calendar_home`.
pub async fn addressbook_home(state: &AppState, user: &str, req: Request) -> Response {
    let depth = parse_depth(req.headers());
    let mode = read_propfind_mode(req).await;

    let books = match state.addressbooks.list_addressbooks(user).await {
        Ok(b) => b,
        Err(err) => return storage_error_response(err),
    };
    let entries: Vec<AddressbookEntry> = books
        .iter()
        .map(|b| AddressbookEntry {
            href: format!("/remote.php/dav/addressbooks/{user}/{}/", b.id),
            display_name: &b.display_name,
            ctag: &b.ctag,
        })
        .collect();

    let home_href = format!("/remote.php/dav/addressbooks/{user}/");
    let xml =
        response::addressbook_home_multistatus(&home_href, &entries, depth == Depth::One, &mode);
    xml_response(xml)
}

/// PROPFIND on a single addressbook collection: Depth 0 answers only the
/// collection's own properties; Depth 1 also lists every contact resource
/// directly under it (href + getetag only) -- see
/// `crate::xml::response::addressbook_collection_multistatus`'s doc
/// comment for why Depth 1 enumeration matters here as much as it did for
/// CalDAV.
pub async fn addressbook_collection(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    req: Request,
) -> Response {
    let depth = parse_depth(req.headers());
    let mode = read_propfind_mode(req).await;

    let info = match state
        .addressbooks
        .get_addressbook(user, addressbook_id)
        .await
    {
        Ok(b) => b,
        Err(StorageError::NotFound) => {
            return (StatusCode::NOT_FOUND, "no such addressbook").into_response()
        }
        Err(err) => return storage_error_response(err),
    };

    let entry = AddressbookEntry {
        href: format!("/remote.php/dav/addressbooks/{user}/{addressbook_id}/"),
        display_name: &info.display_name,
        ctag: &info.ctag,
    };

    let children = if depth == Depth::One {
        match list_contact_children(state, user, addressbook_id).await {
            Ok(c) => Some(c),
            Err(err) => return storage_error_response(err),
        }
    } else {
        None
    };

    xml_response(response::addressbook_collection_multistatus(
        &entry,
        children.as_deref(),
        &mode,
    ))
}

async fn list_contact_children(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
) -> Result<Vec<(String, String)>, StorageError> {
    let contacts = state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await?;
    let listing = contacts.list_dir(&DavPath::root()).await?;
    let mut children = Vec::with_capacity(listing.len());
    for entry in listing {
        if entry.metadata.is_dir() {
            continue;
        }
        let etag = contacts.etag_for(&entry.path, &entry.metadata).await?;
        let obj = entry.path.name().unwrap_or("").to_string();
        children.push((
            format!("/remote.php/dav/addressbooks/{user}/{addressbook_id}/{obj}"),
            etag,
        ));
    }
    Ok(children)
}
