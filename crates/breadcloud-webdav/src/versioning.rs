//! Snapshots a file's current content into version history
//! (`AppState::versions`, see `breadcloud_storage::versions`)
//! immediately before an overwrite.
//!
//! Shared between the plain PUT path (`handlers::put`) and the
//! chunked-upload finalize path (`handlers::uploads::finalize`) the same
//! way `preconditions` is: both write to a destination in the main tree,
//! and both must retain the same overwrite history, or files above the
//! desktop client's chunking threshold (most of the real dataset, per
//! CLOUD-3) would silently skip versioning while small files got it.
//!
//! First creation of a file is not itself a "previous version" -- there
//! is nothing to snapshot yet, so this is a no-op when `path` does not
//! already exist.

use axum::response::Response;
use breadcloud_storage::{DavPath, StorageError};

use crate::error::storage_error_response;
use crate::state::AppState;

/// Call before writing new content to `path`. If `path` already names a
/// file, its current content is captured as a new version before the
/// caller's write can replace it. Returns `Err` (a ready-to-send
/// response) only for a genuine I/O failure reading the existing content
/// or writing the snapshot -- callers should short-circuit the request
/// with it rather than proceeding with an overwrite that could not be
/// safely versioned.
pub async fn snapshot_before_overwrite(state: &AppState, path: &DavPath) -> Result<(), Response> {
    match state.storage.metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            Err(storage_error_response(StorageError::IsADirectory))
        }
        Ok(_) => state
            .versions
            .snapshot_file(path, &state.storage.resolve(path))
            .await
            .map(|_| ())
            .map_err(storage_error_response),
        // First creation: nothing to snapshot.
        Err(StorageError::NotFound) => Ok(()),
        Err(err) => Err(storage_error_response(err)),
    }
}
