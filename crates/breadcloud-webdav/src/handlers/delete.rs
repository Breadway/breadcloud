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
        Ok(()) => {
            // CLOUD-13: best-effort, see `breadcloud_events` module docs.
            // `bytes` is omitted: `path` may name a directory, and unlike
            // the file-op paths there is no size already in hand here
            // (only `move_to_trash`'s own recursive walk would have it,
            // not worth paying for again just for this).
            breadcloud_events::file_synced(
                state.owner_label(),
                &path.to_request_path(),
                breadcloud_events::FileOp::Delete,
                None,
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(resp) => resp,
    }
}
