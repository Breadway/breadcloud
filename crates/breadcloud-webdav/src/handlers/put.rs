use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, WriteOutcome};

use crate::error::storage_error_response;
use crate::state::AppState;

/// Safety cap on a buffered PUT body. Individual files in the target tree
/// can be multiple gigabytes; buffering the whole request in memory is a
/// known limitation of this issue's scope (streaming/chunked upload is
/// CLOUD-3), not an oversight, so the cap is generous rather than tight.
const MAX_BODY_BYTES: usize = 20 * 1024 * 1024 * 1024;

pub async fn handle(state: &AppState, path: &DavPath, req: Request) -> Response {
    if path.is_root() {
        return (StatusCode::CONFLICT, "cannot PUT to the storage root").into_response();
    }

    let bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read request body").into_response(),
    };

    // RFC 4918 9.7.1: PUT creates the resource if it does not exist and
    // overwrites it if it does; the parent collection must already
    // exist, which `write_bytes` enforces (409 Conflict otherwise).
    match state.storage.write_bytes(path, &bytes).await {
        Ok(WriteOutcome::Created) => StatusCode::CREATED.into_response(),
        Ok(WriteOutcome::Overwritten) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => storage_error_response(err),
    }
}
