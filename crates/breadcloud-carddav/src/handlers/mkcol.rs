use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::StorageError;
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::read_body;
use crate::state::AppState;
use crate::xml::request::parse_mkcol_body;

/// `MKCOL /remote.php/dav/addressbooks/<user>/<addressbook id>/`: create
/// a new addressbook collection. CardDAV (RFC 6352) has no
/// `MKCALENDAR`-equivalent verb of its own -- addressbook creation is a
/// plain `MKCOL`, optionally carrying an RFC 5689 extended-MKCOL body
/// that sets `resourcetype`/`displayname` in the same request (see
/// `crate::xml::request::parse_mkcol_body`'s doc comment for why this
/// server does not require or validate that body: every `MKCOL` under
/// this path prefix already unambiguously means "create an addressbook",
/// since nothing else lives under it). `addressbook id` is whatever the
/// client (or a human with curl) names it, matching `MKCALENDAR`'s own
/// "not a server-generated id" stance.
pub async fn handle(state: &AppState, user: &str, addressbook_id: &str, req: Request) -> Response {
    let body = read_body(req).await;
    let display_name = parse_mkcol_body(&body).unwrap_or_else(|| addressbook_id.to_string());

    match state
        .addressbooks
        .create_addressbook(user, addressbook_id, &display_name)
        .await
    {
        Ok(_) => StatusCode::CREATED.into_response(),
        // RFC 4918 9.3.1 (MKCOL on an existing resource is 405), the same
        // status breadcloud_webdav::handlers::mkcol and
        // breadcloud_caldav::handlers::mkcalendar both give.
        Err(StorageError::AlreadyExists) => {
            (StatusCode::METHOD_NOT_ALLOWED, "addressbook already exists").into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
