use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::DavPath;

use crate::state::AppState;

pub async fn handle(state: &AppState, path: &DavPath) -> Response {
    if path.is_root() {
        return (StatusCode::FORBIDDEN, "cannot DELETE the storage root").into_response();
    }

    // Soft delete (CLOUD-5): move the resource (and its version history,
    // if any) into trash instead of actually removing it. See
    // `crate::trash::move_to_trash` and the workspace README for the
    // retention-cap and version-history interaction decisions.
    match crate::trash::move_to_trash(state, path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(resp) => resp,
    }
}
