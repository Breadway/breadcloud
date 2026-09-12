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
        Ok(()) => {
            // Version history (CLOUD-4) for a deleted resource is
            // discarded along with it -- for a directory this recursively
            // discards every descendant's history too (see
            // VersionStore::delete_all). There is no trash yet (CLOUD-5)
            // to make keeping a deleted file's history meaningful, and
            // leaving it behind indefinitely would be a leak, so this
            // server does not. Best-effort: a failure here is logged but
            // does not turn an already-successful DELETE into an error
            // response.
            if let Err(err) = state.versions.delete_all(path).await {
                tracing::warn!("failed to purge version history for {path}: {err}");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
