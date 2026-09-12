//! Per-file version history (CLOUD-4): a capped, content-addressed-by-time
//! store of a file's previous content, kept entirely separate from the
//! served tree.
//!
//! Layout mirrors the live tree's own shape: a live file at `/a/b/c.txt`
//! has its retained versions stored as sibling files under
//! `<versions root>/a/b/c.txt/`, one file per version, named by a
//! zero-padded nanosecond timestamp so a plain lexicographic directory
//! listing already sorts oldest to newest. This is the same "separate
//! `Storage`-like root, out of band from the main tree" pattern
//! `AppState::uploads` uses for in-progress chunked uploads (see
//! `breadcloud_webdav::handlers::uploads`), applied to version history
//! instead of upload staging -- the goal in both cases is that a PROPFIND
//! of the real tree can never see this data.
//!
//! Retention is capped two ways, both enforced on every
//! [`VersionStore::snapshot`]:
//!
//! - a flat count cap ([`DEFAULT_MAX_VERSIONS_PER_FILE`]): the oldest
//!   version is evicted once a file has more than this many retained;
//! - a total-size cap ([`DEFAULT_MAX_VERSION_SET_BYTES`]) on one file's
//!   whole retained set, independent of the count cap, so a handful of
//!   versions of one enormous file cannot balloon storage the count cap
//!   alone would allow. The size cap never evicts the last remaining
//!   version even if that version alone exceeds it -- see
//!   [`VersionStore::prune`].
//!
//! Neither cap is user-configurable (no policy engine): both are fixed
//! constants, documented here and in the workspace README, matching the
//! issue's ask for "a generous but bounded" cap rather than a
//! configurable retention system nobody asked for.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::fs;

use crate::error::StorageError;
use crate::path::DavPath;

/// Default cap on how many past versions of a single file are retained.
/// "Generous but bounded" per the issue: current real usage across the
/// whole tree is about 615 MB, so retaining ten full copies of a typical
/// file's history is not a meaningful amount of extra storage, while
/// still being a hard, finite number (never unlimited) for a file that
/// gets rewritten often. This is a flat cap, not Nextcloud's own tapering
/// "keep N recent, then fewer older ones at wider intervals" scheme --
/// the tapering logic is real machinery for a benefit (very long-tail
/// history) nothing here asked for; a single flat "last N" is the
/// simplest thing that satisfies the issue.
pub const DEFAULT_MAX_VERSIONS_PER_FILE: usize = 10;

/// Default cap on the total size of one file's retained version set,
/// independent of the count cap above. Without this, repeatedly
/// overwriting one very large file (a video, a disk image, a big
/// archive) could make its version history balloon far past what the
/// live file itself costs -- ten retained copies of a 2 GiB file would be
/// 20 GiB of history for that one file alone. 500 MiB is generous for the
/// document/photo/config churn this cap is really meant for, while still
/// bounding the pathological case: for a genuinely huge file, this is the
/// cap that actually bites, pruning down toward a single retained
/// version rather than ten.
pub const DEFAULT_MAX_VERSION_SET_BYTES: u64 = 500 * 1024 * 1024;

/// One retained version of a file.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    /// Opaque, sortable version identifier (a zero-padded nanosecond
    /// timestamp). Stable once created; used both as the DAV surface's
    /// version-resource name and as the internal lookup key for
    /// [`VersionStore::read`].
    pub id: String,
    pub len: u64,
    /// When this version was captured (i.e. when the overwrite that
    /// pushed this content into history happened), not the original
    /// file's own last-modified time.
    pub modified: SystemTime,
}

/// Version history storage, sandboxed to its own root directory
/// (distinct from the main served tree and from chunked-upload staging).
/// Cheap to clone in spirit (it holds only a path and two `Copy` caps),
/// mirroring [`crate::Storage`]'s own cloning story.
#[derive(Debug, Clone)]
pub struct VersionStore {
    root: PathBuf,
    max_versions: usize,
    max_total_bytes: u64,
}

impl VersionStore {
    /// A version store using the default caps documented on
    /// [`DEFAULT_MAX_VERSIONS_PER_FILE`] / [`DEFAULT_MAX_VERSION_SET_BYTES`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(
            root,
            DEFAULT_MAX_VERSIONS_PER_FILE,
            DEFAULT_MAX_VERSION_SET_BYTES,
        )
    }

    /// A version store with explicit caps, mainly for tests that want to
    /// exercise eviction without writing hundreds of megabytes of test
    /// data.
    pub fn with_limits(
        root: impl Into<PathBuf>,
        max_versions: usize,
        max_total_bytes: u64,
    ) -> Self {
        VersionStore {
            root: root.into(),
            max_versions,
            max_total_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_for(&self, path: &DavPath) -> PathBuf {
        let mut full = self.root.clone();
        for seg in path.segments() {
            full.push(seg);
        }
        full
    }

    /// Snapshot `data` (a file's content, read immediately before an
    /// overwrite) as a new version of `path`, then evict old versions per
    /// the count and total-size caps.
    ///
    /// Callers decide whether a snapshot is warranted at all: this always
    /// creates one. In practice only `breadcloud_webdav::versioning`
    /// calls this, and only when `path` already existed before the write
    /// that is about to replace it -- first creation is not a "previous
    /// version".
    pub async fn snapshot(&self, path: &DavPath, data: &[u8]) -> Result<VersionInfo, StorageError> {
        let dir = self.dir_for(path);
        fs::create_dir_all(&dir).await?;

        let id = self.next_version_id(&dir).await?;
        fs::write(dir.join(&id), data).await?;

        let info = VersionInfo {
            id,
            len: data.len() as u64,
            modified: SystemTime::now(),
        };

        self.prune(&dir).await?;
        Ok(info)
    }

    /// A zero-padded (fixed-width, so lexicographic order matches numeric
    /// order) nanosecond timestamp. Bumped forward a nanosecond at a time
    /// on the rare collision (two snapshots of the same file landing in
    /// the same clock tick), so ids stay unique without a shared counter.
    async fn next_version_id(&self, dir: &Path) -> Result<String, StorageError> {
        let mut nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        loop {
            let id = format!("{nanos:020}");
            if fs::metadata(dir.join(&id)).await.is_err() {
                return Ok(id);
            }
            nanos += 1;
        }
    }

    /// List retained versions of `path`, oldest first. Empty (not an
    /// error) if `path` has no version history.
    pub async fn list(&self, path: &DavPath) -> Result<Vec<VersionInfo>, StorageError> {
        let dir = self.dir_for(path);
        let mut entries = match read_entries(&dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(entries)
    }

    /// Open one specific retained version for reading, e.g. for the
    /// versions DAV surface's GET, or for a caller restoring it (read the
    /// old bytes here, then `Storage::write_bytes` them back onto the
    /// live path -- restoring is just another overwrite, so it gets
    /// snapshotted into history too, same as any other write).
    pub async fn read(
        &self,
        path: &DavPath,
        version_id: &str,
    ) -> Result<(fs::File, u64), StorageError> {
        let file_path = self.dir_for(path).join(version_id);
        let meta = fs::metadata(&file_path).await.map_err(map_not_found)?;
        if meta.is_dir() {
            return Err(StorageError::IsADirectory);
        }
        let file = fs::File::open(&file_path).await?;
        Ok((file, meta.len()))
    }

    /// Discard every retained version of `path`. For a file, removes its
    /// version directory; for a collection, removes that directory
    /// recursively, which (since version directories mirror the live
    /// tree's shape) discards every descendant file's history in one
    /// call too.
    ///
    /// Used when the live resource at `path` is deleted: see the
    /// workspace README for why this server does not try to keep a
    /// deleted file's version history around (that would only be
    /// meaningful paired with trash/undelete, a separate later issue --
    /// CLOUD-5 -- not this one). A no-op, not an error, if `path` has no
    /// history yet.
    pub async fn delete_all(&self, path: &DavPath) -> Result<(), StorageError> {
        let dir = self.dir_for(path);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Move `path`'s entire retained-version subtree out to the external
    /// directory `dest_dir`, outside this store entirely. Used when a
    /// file (or, since version directories mirror the live tree's shape,
    /// a whole directory's worth of descendant histories in one call) is
    /// soft-deleted, so its history can travel into trash alongside it
    /// instead of being purged outright (see CLOUD-5's `TrashStore` and
    /// `breadcloud_webdav::trash`). Returns `true` if there was anything
    /// to move (and `dest_dir` now holds it), `false` if `path` had no
    /// history (a no-op; `dest_dir` is left untouched).
    ///
    /// Moves via [`crate::move_path`], the safe replace primitive
    /// `TrashStore`'s own payload move also uses (see its doc comment for
    /// the full safety argument): `BREADCLOUD_VERSIONS_ROOT` is exactly as
    /// independently configurable as the trash root, so this move can
    /// cross filesystems too, and this is also the move a failed restore's
    /// rollback uses to undo an already-succeeded [`VersionStore::move_in`]
    /// -- both need the same guarantee that a failure never destroys real
    /// content on either side, not just a cross-filesystem fallback.
    pub async fn move_out(&self, path: &DavPath, dest_dir: &Path) -> Result<bool, StorageError> {
        let dir = self.dir_for(path);
        if fs::metadata(&dir).await.is_err() {
            return Ok(false);
        }
        crate::move_path(&dir, dest_dir).await?;
        Ok(true)
    }

    /// The inverse of [`VersionStore::move_out`]: bring a previously
    /// extracted subtree at `src_dir` back in as `path`'s retained-version
    /// set (used when restoring a soft-deleted resource from trash). A
    /// no-op if `src_dir` does not exist (the resource had no history when
    /// it was trashed, or a prior call already moved it in).
    ///
    /// Moves via [`crate::move_path`], which handles a pre-existing
    /// `path` version directory itself: unlike [`VersionStore::move_all`]
    /// (an ordinary rename on a DAV `MOVE`, where discarding the
    /// destination's own prior history outright is the correct, intended
    /// behavior), this call's destination can legitimately already hold
    /// real content -- most importantly, a leftover from a previous
    /// restore attempt whose own rollback (a [`VersionStore::move_out`]
    /// call, the reverse of this one) failed partway. `move_path` never
    /// discards existing destination content until the new content is
    /// fully staged and verified, so that case is handled correctly here
    /// without this method needing to reason about it itself. Callers must
    /// treat an ordinary `Err` here as the version history genuinely not
    /// having moved (it is still at `src_dir`, untouched) and must not go
    /// on to discard `src_dir`'s containing trash entry as if the move had
    /// succeeded. The one exception is `StorageError::RecoveryFailed`:
    /// `move_path`'s own unwind of a failed move can itself fail, in which
    /// case the content is stranded at the stage/backup path the error
    /// names rather than safely back at `src_dir` -- callers must check
    /// for that variant distinctly and must not assume `src_dir` still
    /// holds the history in that case.
    pub async fn move_in(&self, path: &DavPath, src_dir: &Path) -> Result<(), StorageError> {
        if fs::metadata(src_dir).await.is_err() {
            return Ok(());
        }
        let dir = self.dir_for(path);
        crate::move_path(src_dir, &dir).await?;
        Ok(())
    }

    /// Carry `from`'s version history over to `to` on a rename (MOVE).
    /// A no-op if `from` has no history yet. If `to` already has its own
    /// version history (its previous content is being replaced or
    /// removed by this same move), that history is discarded first: it
    /// belonged to content this move is removing, not to anything still
    /// reachable afterward.
    pub async fn move_all(&self, from: &DavPath, to: &DavPath) -> Result<(), StorageError> {
        let from_dir = self.dir_for(from);
        if fs::metadata(&from_dir).await.is_err() {
            return Ok(());
        }
        let to_dir = self.dir_for(to);
        if let Some(parent) = to_dir.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Best-effort: the common case is that `to` has no version
        // directory yet, which is a NotFound this crate treats the same
        // as "nothing to discard".
        let _ = fs::remove_dir_all(&to_dir).await;
        fs::rename(&from_dir, &to_dir).await?;
        Ok(())
    }

    /// Evict the oldest versions in `dir` until both the count cap and
    /// the total-size cap are satisfied. The size cap never evicts the
    /// single newest version even if that version alone exceeds the cap:
    /// a huge file still ends up with at least one retained prior
    /// version rather than none, and the cap's real job -- keeping a
    /// huge file's history from ballooning to many multiples of its own
    /// size -- is done either way.
    async fn prune(&self, dir: &Path) -> Result<(), StorageError> {
        let mut entries = read_entries(dir).await?;
        entries.sort_by(|a, b| a.id.cmp(&b.id));

        while entries.len() > self.max_versions {
            evict_oldest(dir, &mut entries).await?;
        }

        let mut total: u64 = entries.iter().map(|e| e.len).sum();
        while total > self.max_total_bytes && entries.len() > 1 {
            total = total.saturating_sub(entries[0].len);
            evict_oldest(dir, &mut entries).await?;
        }

        Ok(())
    }
}

async fn read_entries(dir: &Path) -> std::io::Result<Vec<VersionInfo>> {
    let mut out = Vec::new();
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let meta = entry.metadata().await?;
        if !meta.is_file() {
            continue;
        }
        out.push(VersionInfo {
            id: entry.file_name().to_string_lossy().into_owned(),
            len: meta.len(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
        });
    }
    Ok(out)
}

async fn evict_oldest(dir: &Path, entries: &mut Vec<VersionInfo>) -> Result<(), StorageError> {
    if entries.is_empty() {
        return Ok(());
    }
    let oldest = entries.remove(0);
    fs::remove_file(dir.join(&oldest.id)).await?;
    Ok(())
}

fn map_not_found(e: std::io::Error) -> StorageError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound
    } else {
        StorageError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(max_versions: usize, max_total_bytes: u64) -> (TempDir, VersionStore) {
        let dir = TempDir::new().unwrap();
        let store = VersionStore::with_limits(dir.path(), max_versions, max_total_bytes);
        (dir, store)
    }

    #[tokio::test]
    async fn snapshot_then_list_roundtrips() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        store.snapshot(&path, b"v1").await.unwrap();
        store.snapshot(&path, b"v2").await.unwrap();

        let versions = store.list(&path).await.unwrap();
        assert_eq!(versions.len(), 2);
        // Oldest first.
        assert!(versions[0].id < versions[1].id);
    }

    #[tokio::test]
    async fn list_on_unversioned_file_is_empty_not_error() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/never-written.txt").unwrap();
        let versions = store.list(&path).await.unwrap();
        assert!(versions.is_empty());
    }

    #[tokio::test]
    async fn read_returns_the_exact_version_content() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        store.snapshot(&path, b"first content").await.unwrap();
        let versions = store.list(&path).await.unwrap();
        let id = &versions[0].id;

        let (mut file, len) = store.read(&path, id).await.unwrap();
        assert_eq!(len, 13);
        let mut buf = Vec::new();
        use tokio::io::AsyncReadExt;
        file.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"first content");
    }

    #[tokio::test]
    async fn read_missing_version_is_not_found() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        let err = store.read(&path, "00000000000000000000").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound));
    }

    #[tokio::test]
    async fn count_cap_evicts_the_oldest_version() {
        let (_dir, store) = store(3, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        for i in 0..5 {
            store
                .snapshot(&path, format!("content-{i}").as_bytes())
                .await
                .unwrap();
        }

        let versions = store.list(&path).await.unwrap();
        assert_eq!(versions.len(), 3);

        // The three retained versions must be the three most recent
        // writes (content-2, content-3, content-4), not any of the two
        // oldest.
        for id in versions.iter().map(|v| &v.id) {
            let (mut file, _len) = store.read(&path, id).await.unwrap();
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            file.read_to_end(&mut buf).await.unwrap();
            let content = String::from_utf8(buf).unwrap();
            assert!(
                content == "content-2" || content == "content-3" || content == "content-4",
                "unexpected retained content: {content}"
            );
        }
    }

    #[tokio::test]
    async fn size_cap_evicts_oldest_but_keeps_at_least_one() {
        // Total cap smaller than two versions combined, so only the
        // newest should survive each time the cap is exceeded.
        let (_dir, store) = store(10, 15);
        let path = DavPath::parse("/big.bin").unwrap();
        store.snapshot(&path, &[0u8; 10]).await.unwrap();
        store.snapshot(&path, &[1u8; 10]).await.unwrap();

        let versions = store.list(&path).await.unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[tokio::test]
    async fn size_cap_never_evicts_the_last_version_even_if_oversized() {
        let (_dir, store) = store(10, 5);
        let path = DavPath::parse("/huge.bin").unwrap();
        // A single version already larger than the whole cap: it must
        // still be retained (better one oversized version than zero
        // history), not immediately evicted.
        store.snapshot(&path, &[0u8; 50]).await.unwrap();

        let versions = store.list(&path).await.unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[tokio::test]
    async fn delete_all_removes_the_whole_history() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        store.snapshot(&path, b"v1").await.unwrap();
        store.snapshot(&path, b"v2").await.unwrap();

        store.delete_all(&path).await.unwrap();
        assert!(store.list(&path).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_all_on_unversioned_path_is_not_an_error() {
        let (_dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/never-written.txt").unwrap();
        store.delete_all(&path).await.unwrap();
    }

    #[tokio::test]
    async fn delete_all_on_a_directory_path_removes_descendant_history_too() {
        let (_dir, store) = store(10, 1024 * 1024);
        let a = DavPath::parse("/dir/a.txt").unwrap();
        let b = DavPath::parse("/dir/sub/b.txt").unwrap();
        store.snapshot(&a, b"v1").await.unwrap();
        store.snapshot(&b, b"v1").await.unwrap();

        let dir_path = DavPath::parse("/dir").unwrap();
        store.delete_all(&dir_path).await.unwrap();

        assert!(store.list(&a).await.unwrap().is_empty());
        assert!(store.list(&b).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn move_all_carries_history_to_the_new_path() {
        let (_dir, store) = store(10, 1024 * 1024);
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        store.snapshot(&from, b"v1").await.unwrap();
        store.snapshot(&from, b"v2").await.unwrap();

        store.move_all(&from, &to).await.unwrap();

        assert!(store.list(&from).await.unwrap().is_empty());
        assert_eq!(store.list(&to).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn move_all_on_unversioned_source_is_a_no_op() {
        let (_dir, store) = store(10, 1024 * 1024);
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        store.move_all(&from, &to).await.unwrap();
        assert!(store.list(&to).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn move_out_then_move_in_roundtrips_history() {
        let (dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        store.snapshot(&path, b"v1").await.unwrap();
        store.snapshot(&path, b"v2").await.unwrap();

        let external = dir.path().join("trash-entry").join("versions");
        let moved = store.move_out(&path, &external).await.unwrap();
        assert!(moved);
        assert!(store.list(&path).await.unwrap().is_empty());
        assert!(external.is_dir());

        store.move_in(&path, &external).await.unwrap();
        assert_eq!(store.list(&path).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn move_out_on_unversioned_path_is_a_no_op() {
        let (dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/never-written.txt").unwrap();
        let external = dir.path().join("trash-entry").join("versions");

        let moved = store.move_out(&path, &external).await.unwrap();
        assert!(!moved);
        assert!(!external.exists());
    }

    #[tokio::test]
    async fn move_in_on_missing_source_is_a_no_op() {
        let (dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        let external = dir.path().join("trash-entry").join("versions");

        store.move_in(&path, &external).await.unwrap();
        assert!(store.list(&path).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn move_out_and_move_in_survive_the_cross_device_fallback() {
        // `VersionStore::move_out`/`move_in` both go through
        // `crate::move_path`, which always tries a plain rename first --
        // a test on one real filesystem can never make that fail with a
        // genuine EXDEV. Exercise its cross-device fallback
        // (`crate::copy_verified`) directly instead, shaped exactly like a
        // real version-history move: a directory of version files, not a
        // single file, since that is what `dir_for` always produces.
        // Unlike the old fallback this replaces, `copy_verified` never
        // removes its source itself -- that is `move_path`'s job, done
        // only after the copy is safely swapped into the real destination
        // -- so this only asserts the copy landed correctly.
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let versions_subdir = src_dir.path().join("a.txt");
        tokio::fs::create_dir_all(&versions_subdir).await.unwrap();
        tokio::fs::write(versions_subdir.join("00000000000000000001"), b"v1")
            .await
            .unwrap();
        tokio::fs::write(versions_subdir.join("00000000000000000002"), b"v2")
            .await
            .unwrap();
        let dest = dest_dir.path().join("trash-entry").join("versions");

        crate::copy_verified(&versions_subdir, &dest).await.unwrap();

        assert_eq!(
            tokio::fs::read(dest.join("00000000000000000001"))
                .await
                .unwrap(),
            b"v1"
        );
        assert_eq!(
            tokio::fs::read(dest.join("00000000000000000002"))
                .await
                .unwrap(),
            b"v2"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn move_in_never_destroys_real_history_when_its_own_rollback_fails_twice() {
        // Reproduces the round-3 CLOUD-5 bug directly at the level it
        // actually lived at: `VersionStore::move_in`'s destination-clobber
        // used to be unconditional (`let _ = fs::remove_dir_all(&dir)`),
        // so it could not tell a genuine pre-existing version directory
        // apart from a partial leftover of a previous, half-failed
        // attempt at the exact same move. This drives the real sequence a
        // restore-then-roll-back-then-retry produces -- two independent
        // forced failures, one on the forward move and one on the
        // rollback that a later step would trigger -- and asserts the
        // real, complete version history is never destroyed at any point
        // along the way, with more than one retained version so a naive
        // "did *something* survive" check could not paper over partial
        // loss.
        use std::os::unix::fs::PermissionsExt;

        let (dir, store) = store(10, 1024 * 1024);
        let path = DavPath::parse("/a.txt").unwrap();
        store.snapshot(&path, b"v1").await.unwrap();
        store.snapshot(&path, b"v2").await.unwrap();

        // "Delete": move the whole history out into a trash-entry-shaped
        // external directory.
        let entry_dir = dir.path().join("trash-entry");
        let external_versions = entry_dir.join("versions");
        tokio::fs::create_dir_all(&entry_dir).await.unwrap();
        assert!(store.move_out(&path, &external_versions).await.unwrap());
        assert!(store.list(&path).await.unwrap().is_empty());

        // First forced failure: block the forward restore move by making
        // the version store's own root (the destination's parent)
        // read-only, the same as a real permissions problem or a
        // read-only mount would.
        let mut root_perms = std::fs::metadata(store.root()).unwrap().permissions();
        root_perms.set_mode(0o555);
        std::fs::set_permissions(store.root(), root_perms.clone()).unwrap();

        assert!(
            store.move_in(&path, &external_versions).await.is_err(),
            "the forced failure must surface as an error"
        );
        assert!(
            store.list(&path).await.unwrap().is_empty(),
            "a failed forward move must leave the live side empty"
        );

        // Fix permissions and retry: the forward move must now fully
        // succeed.
        root_perms.set_mode(0o755);
        std::fs::set_permissions(store.root(), root_perms).unwrap();
        store.move_in(&path, &external_versions).await.unwrap();
        assert_eq!(store.list(&path).await.unwrap().len(), 2);

        // Second, independent forced failure: simulate a later payload
        // move failing (as the real restore handler does) by forcing the
        // *rollback* -- moving the live-mirror history just restored above
        // back out to the trash entry -- to fail partway. Block writes to
        // the trash entry directory itself, the same class of real-world
        // fault (a permissions problem, or an I/O hiccup partway through a
        // genuine cross-device copy) that a rollback is exactly as exposed
        // to as the move it is undoing.
        let mut entry_perms = std::fs::metadata(&entry_dir).unwrap().permissions();
        entry_perms.set_mode(0o555);
        std::fs::set_permissions(&entry_dir, entry_perms.clone()).unwrap();

        assert!(
            store.move_out(&path, &external_versions).await.is_err(),
            "the forced rollback failure must surface as an error"
        );

        // The critical property this test exists for: the real, complete
        // version history must still be findable, intact, at the
        // live-mirror path the forward move put it at. This is exactly
        // what the round-3 bug lost -- the next `move_in` unconditionally
        // discarded "the destination has content" without asking whether
        // that content was legitimate, destroying it.
        assert_eq!(
            store.list(&path).await.unwrap().len(),
            2,
            "a failed rollback must never destroy the real history it failed to move"
        );

        // Fix permissions and retry the rollback: it must now fully
        // succeed, landing exactly the original two versions in the trash
        // entry, no more, no less (no duplication from the earlier failed
        // attempt either).
        entry_perms.set_mode(0o755);
        std::fs::set_permissions(&entry_dir, entry_perms).unwrap();
        store.move_out(&path, &external_versions).await.unwrap();
        assert!(store.list(&path).await.unwrap().is_empty());

        // Bring it back one final time to confirm the whole roundtrip is
        // intact end to end, not just "some files survived somewhere".
        store.move_in(&path, &external_versions).await.unwrap();
        let versions = store.list(&path).await.unwrap();
        assert_eq!(versions.len(), 2);
        use tokio::io::AsyncReadExt;
        let mut contents = Vec::new();
        for v in &versions {
            let (mut file, _len) = store.read(&path, &v.id).await.unwrap();
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await.unwrap();
            contents.push(String::from_utf8(buf).unwrap());
        }
        assert!(contents.contains(&"v1".to_string()));
        assert!(contents.contains(&"v2".to_string()));
    }

    #[tokio::test]
    async fn move_all_discards_the_destinations_own_prior_history() {
        let (_dir, store) = store(10, 1024 * 1024);
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        store.snapshot(&to, b"b-old-v1").await.unwrap();
        store.snapshot(&from, b"a-v1").await.unwrap();

        store.move_all(&from, &to).await.unwrap();

        let versions = store.list(&to).await.unwrap();
        assert_eq!(versions.len(), 1);
    }
}
