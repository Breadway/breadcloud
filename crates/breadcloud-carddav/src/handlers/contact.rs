use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, WriteOutcome};
use breadcloud_webdav::error::storage_error_response;
use breadcloud_webdav::preconditions;

use crate::handlers::read_body;
use crate::state::AppState;
use crate::vcard;

fn dav_path(obj: &str) -> Option<DavPath> {
    if obj.chars().any(char::is_control) {
        return None;
    }
    DavPath::parse(&format!("/{obj}")).ok()
}

/// `GET /remote.php/dav/addressbooks/<user>/<addressbook id>/<object>`:
/// fetch one contact's raw vCard content. Not streamed, same reasoning as
/// `breadcloud_caldav::handlers::event::get`: a single `.vcf` object is
/// at most a few kilobytes.
pub async fn get(state: &AppState, user: &str, addressbook_id: &str, obj: &str) -> Response {
    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };
    let bytes = match contacts.read_bytes(&path).await {
        Ok(b) => b,
        Err(err) => return storage_error_response(err),
    };
    let etag = match contacts.etag(&path).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/vcard; charset=utf-8")
        .header("ETag", etag)
        .body(Body::from(bytes))
        .unwrap()
}

/// `PUT /remote.php/dav/addressbooks/<user>/<addressbook id>/<object>`:
/// create or overwrite one contact (an individual or a group vCard --
/// see `crate::vcard`'s module doc comment, both are ordinary resources
/// here). Requires the body to at least parse a `UID`, matching
/// `breadcloud_caldav::handlers::event::put`'s own requirement for
/// calendar objects; does not require `<object>` to equal `<uid>.vcf`,
/// same leniency reasoning as that handler.
pub async fn put(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    obj: &str,
    req: Request,
) -> Response {
    if !state
        .addressbooks
        .addressbook_exists(user, addressbook_id)
        .await
    {
        // RFC 4918 9.7.1 (inherited by CardDAV): PUT into a collection
        // that does not exist is 409 Conflict, not 404.
        return (StatusCode::CONFLICT, "no such addressbook").into_response();
    }
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };

    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };
    let headers = req.headers().clone();
    let bytes = read_body(req).await;
    if vcard::extract_uid(&bytes).is_none() {
        return (StatusCode::BAD_REQUEST, "vCard has no UID").into_response();
    }
    let _resource_gate = match state
        .addressbooks
        .resource_gate(user, addressbook_id, obj)
        .await
    {
        Ok(guard) => guard,
        Err(error) => return storage_error_response(error),
    };
    if let Some(conflict) =
        preconditions::check_storage(&contacts, Some(user), &path, &headers).await
    {
        return conflict;
    }

    let (outcome, etag) = match contacts.write_bytes(&path, &bytes).await {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

    if let Err(err) = state
        .addressbooks
        .record_change(user, addressbook_id, obj, false)
        .await
    {
        tracing::warn!(
            "failed to record addressbook change for {user}/{addressbook_id}/{obj}: {err}; \
             the contact itself was written successfully, but getctag/sync-collection may \
             lag until the next change is recorded"
        );
    }

    let status = match outcome {
        WriteOutcome::Created => StatusCode::CREATED,
        WriteOutcome::Overwritten => StatusCode::NO_CONTENT,
    };
    Response::builder()
        .status(status)
        .header("ETag", etag)
        .body(Body::empty())
        .unwrap()
}

/// `DELETE /remote.php/dav/addressbooks/<user>/<addressbook id>/<object>`.
/// Like `breadcloud_caldav::handlers::event::delete`, this does not go
/// through trash: trash is a file-tree concept, and a contact is a
/// deliberately separate, simpler resource model with no versioning or
/// soft-delete asked for by this issue.
pub async fn delete(
    state: &AppState,
    user: &str,
    addressbook_id: &str,
    obj: &str,
    headers: &HeaderMap,
) -> Response {
    let contacts = match state
        .addressbooks
        .contacts_storage(user, addressbook_id)
        .await
    {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };
    let _resource_gate = match state
        .addressbooks
        .resource_gate(user, addressbook_id, obj)
        .await
    {
        Ok(guard) => guard,
        Err(error) => return storage_error_response(error),
    };
    if let Some(conflict) =
        preconditions::check_storage(&contacts, Some(user), &path, headers).await
    {
        return conflict;
    }
    match contacts.delete(&path).await {
        Ok(()) => {
            if let Err(err) = state
                .addressbooks
                .record_change(user, addressbook_id, obj, true)
                .await
            {
                tracing::warn!(
                    "failed to record addressbook deletion for {user}/{addressbook_id}/{obj}: {err}"
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
