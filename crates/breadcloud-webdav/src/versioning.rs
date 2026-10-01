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

use breadcloud_storage::{DavPath, StorageError, VersionInfo};

use crate::state::AppState;

/// Call before writing new content to `path`. If `path` already names a
/// file, its current content is captured as a new version before the
/// caller's write can replace it. Returns a storage error only for a
/// genuine I/O failure reading the existing content
/// or writing the snapshot -- callers should short-circuit the request
/// with it rather than proceeding with an overwrite that could not be
/// safely versioned.
pub async fn snapshot_before_overwrite(
    state: &AppState,
    path: &DavPath,
) -> Result<Option<VersionInfo>, StorageError> {
    let existing = match state.storage.read_bytes(path).await {
        Ok(bytes) => bytes,
        // First creation: nothing to snapshot.
        Err(StorageError::NotFound) => return Ok(None),
        Err(err) => return Err(err),
    };

    state
        .versions
        .snapshot_pending(path, &existing)
        .await
        .map(Some)
}

/// Finalize retention only after the replacement write commits. Pruning is
/// best-effort here because the live write has already succeeded.
pub async fn finish_snapshot(state: &AppState, path: &DavPath, snapshot: Option<VersionInfo>) {
    if snapshot.is_some() {
        if let Err(err) = state.versions.prune_path(path).await {
            tracing::warn!("failed to prune version history for {path}: {err}");
        }
    }
}

/// Remove a pending version after its corresponding replacement write
/// failed, preserving all history that existed before the request.
pub async fn discard_snapshot(state: &AppState, path: &DavPath, snapshot: Option<VersionInfo>) {
    if let Some(snapshot) = snapshot {
        if let Err(err) = state.versions.delete_version(path, &snapshot.id).await {
            tracing::error!("failed to remove pending version for {path}: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{discard_snapshot, snapshot_before_overwrite};
    use crate::state::AppState;
    use breadcloud_storage::{DavPath, Storage, TrashStore, VersionStore};
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn failed_write_discards_pending_snapshot_without_evicting_history() {
        let storage_dir = TempDir::new().unwrap();
        let uploads_dir = TempDir::new().unwrap();
        let versions_dir = TempDir::new().unwrap();
        let trash_dir = TempDir::new().unwrap();
        let versions = VersionStore::with_limits(versions_dir.path(), 1, 1024);
        let state = AppState::new(
            Storage::new(storage_dir.path()),
            Storage::new(uploads_dir.path()),
            versions,
            TrashStore::new(trash_dir.path()),
        );
        let parent = DavPath::parse("/folder").unwrap();
        let path = DavPath::parse("/folder/file.txt").unwrap();
        state.storage.mkcol(&parent).await.unwrap();
        state.storage.write_bytes(&path, b"current").await.unwrap();
        let retained = state
            .versions
            .snapshot(&path, b"prior history")
            .await
            .unwrap();

        let pending = snapshot_before_overwrite(&state, &path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.versions.list(&path).await.unwrap().len(), 2);

        // Simulate the parent disappearing after snapshot creation, which
        // makes the replacement write fail after versioning has begun.
        tokio::fs::remove_dir_all(state.storage.resolve(&parent))
            .await
            .unwrap();
        assert!(state
            .storage
            .write_bytes(&path, b"replacement")
            .await
            .is_err());
        discard_snapshot(&state, &path, Some(pending)).await;

        let remaining = state.versions.list(&path).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, retained.id);
        let (mut prior, _) = state.versions.read(&path, &retained.id).await.unwrap();
        let mut bytes = Vec::new();
        prior.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"prior history");
    }
}
