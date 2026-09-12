use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::DavPath;

use crate::error::storage_error_response;
use crate::state::AppState;

pub async fn handle(state: &AppState, path: &DavPath) -> Response {
    if path.is_root() {
        return (StatusCode::FORBIDDEN, "cannot DELETE the storage root").into_response();
    }
    match state.storage.delete(path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => storage_error_response(err),
    }
}
