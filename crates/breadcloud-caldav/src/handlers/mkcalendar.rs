use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::StorageError;
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::read_body;
use crate::state::AppState;
use crate::xml::request::parse_mkcalendar_body;

/// `MKCALENDAR /remote.php/dav/calendars/<user>/<calendar id>/` (RFC
/// 4791 5.3.1): create a new calendar collection. `calendar id` is
/// whatever the client (or a human with curl) names it; there is no
/// server-generated id scheme the way trash/version entries use one,
/// since a calendar's id is meant to be a stable, human-legible part of
/// its URL (`personal`, `family`, `school`), not an opaque handle.
pub async fn handle(state: &AppState, user: &str, calendar_id: &str, req: Request) -> Response {
    let body = read_body(req).await;
    let (display_name, color) = parse_mkcalendar_body(&body);
    let display_name = display_name.unwrap_or_else(|| calendar_id.to_string());

    match state
        .calendars
        .create_calendar(user, calendar_id, &display_name, color.as_deref())
        .await
    {
        Ok(_) => StatusCode::CREATED.into_response(),
        // RFC 4791 5.3.1.1: MKCALENDAR on an existing resource is 405,
        // the same status RFC 4918 9.3.1 gives MKCOL on an existing
        // resource (see breadcloud_webdav::handlers::mkcol).
        Err(StorageError::AlreadyExists) => {
            (StatusCode::METHOD_NOT_ALLOWED, "calendar already exists").into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
