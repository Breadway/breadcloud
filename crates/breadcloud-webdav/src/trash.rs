//! Coordinates DELETE-into-trash and restore-from-trash across `Storage`,
//! `VersionStore`, and `TrashStore` (CLOUD-5). Shared between
//! `handlers::delete` (soft-delete on DELETE) and `handlers::trashbin`
//! (restore), the same way `versioning` is shared between `handlers::put`
//! and `handlers::uploads::finalize`.
//!
//! Version history follows a soft-deleted resource into trash rather than
//! being purged immediately or retained independently: it moves into the
//! same trash entry on delete ([`move_to_trash`]) and moves back out with
//! it on restore (`handlers::trashbin::restore`). This is the CLOUD-5
//! interaction decision documented in full in the workspace README --
//! the short version is that once trash exists, "keep a deleted file's
//! history around" only makes sense paired with a way to undelete the
//! file it belongs to, which trash now provides; keeping it reachable
//! only through the still-in-trash item (not independently, and not
//! purged outright) avoids both leaking it forever and losing it before
//! the user has decided the deletion is really final.

use breadcloud_storage::{DavPath, EntryKind, StorageError};

use crate::state::AppState;

/// Move the live resource at `path` into trash instead of removing it.
/// Computes its size (recursively, for a directory), moves its
/// filesystem content into a new trash entry, moves its version history
/// (if any) into that same entry, then enforces the retention caps.
///
/// Returns a storage error only if `path` does not exist
/// or the move itself fails; a failure moving version history or
/// enforcing caps afterward is logged but does not turn an
/// already-successful soft-delete into an error response (mirroring how
/// version-history purging was already best-effort before CLOUD-5).
pub async fn move_to_trash(state: &AppState, path: &DavPath) -> Result<(), StorageError> {
    let meta = state.storage.metadata(path).await?;
    let kind = if meta.is_dir() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    let size = if meta.is_dir() {
        directory_size(state, path).await?
    } else {
        meta.len
    };

    let live_fs_path = state.storage.resolve(path);
    let entry = state.trash.trash(&live_fs_path, path, kind, size).await?;

    if let Err(err) = state
        .versions
        .move_out(path, &state.trash.versions_dir(&entry.id))
        .await
    {
        // An ordinary failure here just leaves the version history at its
        // original live-mirror location (`move_path`'s "nothing moved"
        // guarantee), which is why this stays best-effort rather than
        // failing the whole soft-delete: nothing is lost, the history
        // just didn't travel into the trash entry this time.
        // `RecoveryFailed` is a materially worse outcome -- content is
        // stranded at a stage/backup path rather than safely at its
        // original location -- so it gets logged loudly rather than as a
        // routine warning.
        if matches!(err, StorageError::RecoveryFailed { .. }) {
            tracing::error!(
                "failed to move version history for {path} into trash, AND the failure's own \
                 recovery also failed: {err}; this needs manual operator attention"
            );
        } else {
            tracing::warn!("failed to move version history for {path} into trash: {err}");
        }
    }

    if let Err(err) = state.trash.enforce_caps().await {
        tracing::warn!("trash retention cap enforcement failed after deleting {path}: {err}");
    }

    Ok(())
}

/// Recursive total size of every file under (and not including, for a
/// collection) `path`, used to size a directory's trash entry for the
/// size-cap accounting.
async fn directory_size(state: &AppState, path: &DavPath) -> Result<u64, StorageError> {
    let mut total = 0u64;
    for entry in state.storage.list_recursive(path).await? {
        if !entry.metadata.is_dir() {
            total += entry.metadata.len;
        }
    }
    Ok(total)
}
