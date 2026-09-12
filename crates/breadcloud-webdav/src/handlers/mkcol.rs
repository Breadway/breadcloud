use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError};

use crate::error::storage_error_response;
use crate::state::AppState;

pub async fn handle(state: &AppState, path: &DavPath) -> Response {
    match state.storage.mkcol(path).await {
        Ok(()) => StatusCode::CREATED.into_response(),
        // RFC 4918 9.3.1: MKCOL on an existing resource is 405, not 409 or
        // 412 -- distinct from the AlreadyExists handling in MOVE/COPY.
        Err(StorageError::AlreadyExists) => {
            (StatusCode::METHOD_NOT_ALLOWED, "collection already exists").into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
