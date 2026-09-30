//! Soft-delete trash (CLOUD-5): DELETE moves a resource's content here
//! instead of actually removing it from disk, so it can be listed and
//! restored later, subject to an explicit retention cap.
//!
//! Layout: each trashed item gets its own entry directory under the trash
//! root, named by an opaque, sortable id (a zero-padded nanosecond
//! timestamp, same scheme as `crate::versions::VersionStore`'s version
//! ids, and for the same reason: a plain lexicographic listing of the
//! trash root then already sorts oldest-deleted first):
//!
//! ```text
//! <trash root>/<id>/
//!   payload            the trashed file, or the trashed directory's whole subtree
//!   original-path      where it lived before deletion (a DavPath's request-path form)
//!   kind               "file" or "dir"
//!   size               total retained bytes, decimal ASCII (recursive sum for a directory)
//!   versions/          optional: this path's moved-in version history (see below)
//! ```
//!
//! This is the same "separate `Storage`-like root, out of band from the
//! served tree" pattern `AppState::uploads` (chunked-upload staging) and
//! `crate::versions::VersionStore` (CLOUD-4) both already use, applied to
//! whole-resource soft-delete instead: a PROPFIND of the real tree must
//! never be able to see a deleted file still sitting around.
//!
//! This store is deliberately kept unaware of `VersionStore`: the two are
//! coordinated by the webdav layer (`breadcloud_webdav::trash`), the same
//! way `breadcloud_webdav::versioning` coordinates `Storage` and
//! `VersionStore` for snapshot-on-overwrite without either storage type
//! knowing about the other. The `versions` subdirectory above exists only
//! as an agreed-upon place for that coordinator to put a moved-out
//! `VersionStore` subtree; `TrashStore` itself never reads or writes it
//! internally.
//!
//! Retention is capped two ways, both enforced by [`TrashStore::enforce_caps`]:
//!
//! - an age cap ([`DEFAULT_TRASH_MAX_AGE`]): anything older than this is
//!   purged outright;
//! - a total-size cap ([`DEFAULT_TRASH_MAX_TOTAL_BYTES`]) across the whole
//!   trash, evicting the oldest surviving entries first once exceeded.
//!
//! Both exist because the issue this implements is specifically that the
//! real Nextcloud instance this server replaces has *neither* cap and
//! accumulated 94 GB of trash against a 3.6 GB live dataset -- an age cap
//! alone would not have stopped that (repeated small deletions never age
//! out fast enough relative to how often they happen), and a size cap
//! alone would not either (it only bites once the bound is already hit,
//! by which point genuinely old, forgotten deletions are exactly what
//! should have been reclaimed first). Neither cap is user-configurable
//! (no policy engine), matching `VersionStore`'s own caps and the issue's
//! ask for "generous but bounded", not a configurable retention system
//! nobody asked for.
//!
//! Unlike `VersionStore::prune`, cap eviction here has no "keep at least
//! one" exception: it can empty the trash entirely. An empty trash is a
//! perfectly ordinary state (nothing is pending recovery); zero retained
//! versions of a file that is still *live*, by contrast, would defeat the
//! versioning feature for that one file, which is why that cap behaves
//! differently.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::fs;

use crate::error::StorageError;
use crate::path::DavPath;
use crate::EntryKind;

/// Default age cap: anything sitting in trash longer than this is purged.
/// 30 days is generous for "I deleted something by mistake, when do I
/// notice" while still being a hard, finite bound -- the real instance
/// this replaces had no age cap at all, which is exactly the bug this
/// issue fixes.
pub const DEFAULT_TRASH_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default total-size cap across all trashed content combined. Sized
/// relative to this deployment's real live dataset (about 3.6 GB) rather
/// than picked arbitrarily: 16 GiB is generous enough that ordinary
/// day-to-day deletions (and the occasional large folder) comfortably fit
/// well before this bites, while still being a small, bounded multiple of
/// the live tree rather than the unbounded growth (94 GB of trash against
/// 3.6 GB live) the issue calls out as the bug.
pub const DEFAULT_TRASH_MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// One soft-deleted resource sitting in trash.
#[derive(Debug, Clone)]
pub struct TrashEntry {
    /// Opaque, sortable trash id (a zero-padded nanosecond timestamp of
    /// when it was trashed). Stable once created; used as this entry's
    /// lookup key and as the trashbin DAV surface's resource name.
    pub id: String,
    /// Where this resource lived before it was deleted.
    pub original_path: DavPath,
    pub kind: EntryKind,
    /// Total retained bytes (recursive, for a directory). Used for the
    /// size-cap accounting in [`TrashStore::enforce_caps`].
    pub size: u64,
    /// When this item was deleted (derived from `id`, not a separately
    /// stored field).
    pub deleted_at: SystemTime,
}

/// Soft-delete trash storage, sandboxed to its own root directory
/// (distinct from the served tree, the chunked-upload staging area, and
/// version history).
#[derive(Debug, Clone)]
pub struct TrashStore {
    root: PathBuf,
    max_age: Duration,
    max_total_bytes: u64,
}

impl TrashStore {
    /// A trash store using the default caps documented on
    /// [`DEFAULT_TRASH_MAX_AGE`] / [`DEFAULT_TRASH_MAX_TOTAL_BYTES`].
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(root, DEFAULT_TRASH_MAX_AGE, DEFAULT_TRASH_MAX_TOTAL_BYTES)
    }

    /// A trash store with explicit caps, mainly for tests that want to
    /// exercise eviction without waiting real time or writing gigabytes of
    /// test data.
    pub fn with_limits(root: impl Into<PathBuf>, max_age: Duration, max_total_bytes: u64) -> Self {
        TrashStore {
            root: root.into(),
            max_age,
            max_total_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Filesystem path of trash entry `id`'s retained content. `pub` so
    /// the webdav layer can move the live resource's filesystem content
    /// straight into place (see `trash`) and back out again on restore
    /// (see `restore_payload`) without copying through this crate's API a
    /// second time.
    pub fn payload_path(&self, id: &str) -> PathBuf {
        self.entry_dir(id).join("payload")
    }

    /// Filesystem path reserved for trash entry `id`'s moved-in version
    /// history, if any (see the module doc comment). `TrashStore` never
    /// touches this path itself; it exists purely as an agreed location
    /// for the webdav layer's coordinator to use.
    pub fn versions_dir(&self, id: &str) -> PathBuf {
        self.entry_dir(id).join("versions")
    }

    fn original_path_file(&self, id: &str) -> PathBuf {
        self.entry_dir(id).join("original-path")
    }

    fn kind_file(&self, id: &str) -> PathBuf {
        self.entry_dir(id).join("kind")
    }

    fn size_file(&self, id: &str) -> PathBuf {
        self.entry_dir(id).join("size")
    }

    /// A zero-padded (fixed-width, so lexicographic order matches numeric
    /// order) nanosecond timestamp, unique among current entries. Bumped
    /// forward a nanosecond at a time on the rare collision, same scheme
    /// as `VersionStore::next_version_id`.
    async fn next_id(&self) -> Result<String, StorageError> {
        fs::create_dir_all(&self.root).await?;
        let mut nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        loop {
            let id = format!("{nanos:020}");
            match fs::metadata(self.entry_dir(&id)).await {
                Ok(_) => nanos += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(id),
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Move `live_fs_path` (already resolved from the live `Storage`) into
    /// a new trash entry, recording `original_path` / `kind` / `size` so
    /// the item can be listed and restored later.
    ///
    /// Does not enforce the retention caps itself: the webdav layer calls
    /// this, then moves this path's version history (if any) into the
    /// same entry via `versions_dir`, and only then calls
    /// [`TrashStore::enforce_caps`] once -- so a single oversized deletion
    /// cannot get partially evicted mid-assembly.
    pub async fn trash(
        &self,
        live_fs_path: &Path,
        original_path: &DavPath,
        kind: EntryKind,
        size: u64,
    ) -> Result<TrashEntry, StorageError> {
        fs::create_dir_all(&self.root).await?;
        let id = loop {
            let id = self.next_id().await?;
            match fs::create_dir(self.entry_dir(&id)).await {
                Ok(()) => break id,
                // `next_id` is only a hint. Two concurrent trash operations
                // can choose the same id, so the directory creation itself
                // must claim it atomically.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };

        // Publish complete metadata before moving the live payload. A
        // concurrent listing can then either skip this entry while its
        // metadata is incomplete or read a complete entry, but will never
        // fail the whole listing because a metadata file is missing.
        fs::write(
            self.original_path_file(&id),
            original_path.to_request_path(),
        )
        .await?;
        fs::write(self.kind_file(&id), kind_marker(kind)).await?;
        fs::write(self.size_file(&id), size.to_string()).await?;
        crate::move_path(live_fs_path, &self.payload_path(&id)).await?;

        Ok(TrashEntry {
            deleted_at: id_to_time(&id),
            id,
            original_path: original_path.clone(),
            kind,
            size,
        })
    }

    /// List every trashed entry, oldest deletion first.
    pub async fn list(&self) -> Result<Vec<TrashEntry>, StorageError> {
        let mut rd = match fs::read_dir(&self.root).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if !entry.metadata().await?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            match self.get(&id).await {
                Ok(trash_entry) => out.push(trash_entry),
                Err(error) if is_malformed_entry(&error) => {
                    tracing::warn!(trash_entry = %id, error = %error, "skipping malformed trash entry");
                }
                Err(error) => return Err(error),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Look up one trash entry's metadata by id.
    pub async fn get(&self, id: &str) -> Result<TrashEntry, StorageError> {
        let original_path_raw = fs::read_to_string(self.original_path_file(id))
            .await
            .map_err(map_not_found)?;
        let kind_raw = fs::read_to_string(self.kind_file(id))
            .await
            .map_err(map_not_found)?;
        let size_raw = fs::read_to_string(self.size_file(id))
            .await
            .map_err(map_not_found)?;

        let original_path = DavPath::parse(original_path_raw.trim())?;
        let kind = kind_from_marker(kind_raw.trim())?;
        let size = size_raw.trim().parse::<u64>().unwrap_or(0);

        Ok(TrashEntry {
            id: id.to_string(),
            original_path,
            kind,
            size,
            deleted_at: id_to_time(id),
        })
    }

    /// Move trash entry `id`'s payload back out to `dest_fs_path` (the
    /// caller resolves this from the live `Storage`). Leaves the entry's
    /// metadata (and, if present, moved-in version history) behind;
    /// call [`TrashStore::finish_restore`] once the caller has also
    /// pulled any version history back out, to fully remove the
    /// now-empty entry.
    pub async fn restore_payload(
        &self,
        id: &str,
        dest_fs_path: &Path,
    ) -> Result<TrashEntry, StorageError> {
        let entry = self.get(id).await?;
        crate::move_path(&self.payload_path(id), dest_fs_path).await?;
        Ok(entry)
    }

    /// Remove a trash entry's remaining bookkeeping (metadata, and an
    /// empty `versions` directory if there was none to move back) after a
    /// successful [`TrashStore::restore_payload`].
    pub async fn finish_restore(&self, id: &str) -> Result<(), StorageError> {
        self.purge(id).await
    }

    /// Permanently discard a trash entry: its payload, metadata, and any
    /// version history moved in alongside it. Used both for an explicit
    /// early purge (DELETE on the trashbin DAV surface) and for
    /// retention-cap eviction. A no-op, not an error, if `id` is already
    /// gone.
    pub async fn purge(&self, id: &str) -> Result<(), StorageError> {
        match fs::remove_dir_all(self.entry_dir(id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Enforce both retention caps: purge anything older than `max_age`,
    /// then evict the oldest remaining entries (oldest deletion first)
    /// until the total retained size is back at or under
    /// `max_total_bytes`. See the module doc comment for why, unlike
    /// `VersionStore::prune`, this has no "keep at least one" exception.
    ///
    /// Self-contained: the webdav layer calls this after every
    /// trash/restore/purge rather than relying on an external cron job,
    /// since a single-process server has nothing else to run one.
    pub async fn enforce_caps(&self) -> Result<(), StorageError> {
        let entries = self.list().await?; // oldest first
        let now = SystemTime::now();

        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries {
            let age = now.duration_since(entry.deleted_at).unwrap_or_default();
            if age > self.max_age {
                self.purge(&entry.id).await?;
            } else {
                kept.push(entry);
            }
        }

        let mut total: u64 = kept.iter().map(|e| e.size).sum();
        let mut idx = 0;
        while total > self.max_total_bytes && idx < kept.len() {
            total = total.saturating_sub(kept[idx].size);
            self.purge(&kept[idx].id).await?;
            idx += 1;
        }

        Ok(())
    }
}

fn kind_marker(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Directory => "dir",
    }
}

fn kind_from_marker(marker: &str) -> Result<EntryKind, StorageError> {
    match marker {
        "file" => Ok(EntryKind::File),
        "dir" => Ok(EntryKind::Directory),
        other => Err(StorageError::Conflict(format!(
            "unrecognized trash entry kind {other:?}"
        ))),
    }
}

fn id_to_time(id: &str) -> SystemTime {
    match id.parse::<u128>() {
        Ok(nanos) => UNIX_EPOCH + Duration::from_nanos(nanos.min(u64::MAX as u128) as u64),
        Err(_) => UNIX_EPOCH,
    }
}

fn map_not_found(e: std::io::Error) -> StorageError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound
    } else {
        StorageError::Io(e)
    }
}

fn is_malformed_entry(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::NotFound | StorageError::Conflict(_) | StorageError::InvalidPath(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(max_age: Duration, max_total_bytes: u64) -> (TempDir, TempDir, TrashStore) {
        let live = TempDir::new().unwrap();
        let trash_dir = TempDir::new().unwrap();
        let store = TrashStore::with_limits(trash_dir.path(), max_age, max_total_bytes);
        (live, trash_dir, store)
    }

    async fn write_live_file(live: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        let path = live.path().join(name);
        tokio::fs::write(&path, content).await.unwrap();
        path
    }

    const GENEROUS_AGE: Duration = Duration::from_secs(60 * 60 * 24 * 365);
    const GENEROUS_BYTES: u64 = 1024 * 1024 * 1024;

    #[tokio::test]
    async fn trash_then_list_roundtrips() {
        let (live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let live_path = write_live_file(&live, "a.txt", b"hello").await;
        let dav_path = DavPath::parse("/a.txt").unwrap();

        let entry = store
            .trash(&live_path, &dav_path, EntryKind::File, 5)
            .await
            .unwrap();

        assert!(!live_path.exists(), "content must be moved, not copied");
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
        assert_eq!(listed[0].original_path, dav_path);
        assert_eq!(listed[0].size, 5);
        assert_eq!(listed[0].kind, EntryKind::File);
    }

    #[tokio::test]
    async fn list_on_empty_trash_is_empty_not_error() {
        let (_live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_skips_incomplete_entries_and_cap_enforcement_still_runs() {
        let (live, trash_dir, store) = store(GENEROUS_AGE, 5);
        let malformed_dir = trash_dir.path().join("00000000000000000001");
        tokio::fs::create_dir(&malformed_dir).await.unwrap();
        tokio::fs::write(malformed_dir.join("versions"), b"partial restore state")
            .await
            .unwrap();

        let live_path = write_live_file(&live, "a.txt", b"payload").await;
        store
            .trash(
                &live_path,
                &DavPath::parse("/a.txt").unwrap(),
                EntryKind::File,
                7,
            )
            .await
            .unwrap();

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original_path.to_request_path(), "/a.txt");

        store.enforce_caps().await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
        assert!(
            malformed_dir.exists(),
            "listing must not delete malformed data"
        );
    }

    #[tokio::test]
    async fn get_missing_id_is_not_found() {
        let (_live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let err = store.get("00000000000000000000").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound));
    }

    #[tokio::test]
    async fn restore_moves_content_back_and_finish_restore_cleans_up() {
        let (live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let live_path = write_live_file(&live, "a.txt", b"content").await;
        let dav_path = DavPath::parse("/a.txt").unwrap();
        let entry = store
            .trash(&live_path, &dav_path, EntryKind::File, 7)
            .await
            .unwrap();

        let restore_dest = live.path().join("restored.txt");
        store
            .restore_payload(&entry.id, &restore_dest)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&restore_dest).await.unwrap(), b"content");

        // Metadata is still there until finish_restore is called.
        assert!(store.get(&entry.id).await.is_ok());
        store.finish_restore(&entry.id).await.unwrap();
        assert!(store.get(&entry.id).await.is_err());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn trash_moves_a_whole_directory() {
        let (live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let dir_path = live.path().join("photos");
        tokio::fs::create_dir_all(dir_path.join("sub"))
            .await
            .unwrap();
        tokio::fs::write(dir_path.join("sub").join("a.jpg"), b"jpeg-bytes")
            .await
            .unwrap();
        let dav_path = DavPath::parse("/photos").unwrap();

        let entry = store
            .trash(&dir_path, &dav_path, EntryKind::Directory, 10)
            .await
            .unwrap();
        assert!(!dir_path.exists());

        let restore_dest = live.path().join("photos-restored");
        store
            .restore_payload(&entry.id, &restore_dest)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(restore_dest.join("sub").join("a.jpg"))
                .await
                .unwrap(),
            b"jpeg-bytes"
        );
    }

    #[tokio::test]
    async fn purge_removes_the_entry_entirely() {
        let (live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let live_path = write_live_file(&live, "a.txt", b"x").await;
        let dav_path = DavPath::parse("/a.txt").unwrap();
        let entry = store
            .trash(&live_path, &dav_path, EntryKind::File, 1)
            .await
            .unwrap();

        store.purge(&entry.id).await.unwrap();
        assert!(store.get(&entry.id).await.is_err());
    }

    #[tokio::test]
    async fn purge_on_missing_id_is_not_an_error() {
        let (_live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        store.purge("00000000000000000000").await.unwrap();
    }

    #[tokio::test]
    async fn age_cap_evicts_everything_once_it_is_zero() {
        let (live, _trash_dir, store) = store(Duration::ZERO, GENEROUS_BYTES);
        let live_path = write_live_file(&live, "a.txt", b"x").await;
        let dav_path = DavPath::parse("/a.txt").unwrap();
        store
            .trash(&live_path, &dav_path, EntryKind::File, 1)
            .await
            .unwrap();

        store.enforce_caps().await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn size_cap_evicts_oldest_first_and_can_empty_the_trash() {
        let (live, _trash_dir, store) = store(GENEROUS_AGE, 15);
        let a_path = write_live_file(&live, "a.txt", &[0u8; 10]).await;
        store
            .trash(
                &a_path,
                &DavPath::parse("/a.txt").unwrap(),
                EntryKind::File,
                10,
            )
            .await
            .unwrap();
        let b_path = write_live_file(&live, "b.txt", &[0u8; 10]).await;
        store
            .trash(
                &b_path,
                &DavPath::parse("/b.txt").unwrap(),
                EntryKind::File,
                10,
            )
            .await
            .unwrap();

        store.enforce_caps().await.unwrap();
        let remaining = store.list().await.unwrap();
        // Total cap (15) is smaller than both entries combined (20): the
        // older one (a.txt) must be evicted, leaving only the newer one.
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].original_path.to_request_path(), "/b.txt");

        // A cap smaller than even the single remaining entry empties the
        // trash entirely -- no "keep at least one" exception here (see
        // the module doc comment).
        let store = TrashStore::with_limits(store.root(), GENEROUS_AGE, 1);
        store.enforce_caps().await.unwrap();
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cross_device_fallback_copies_a_file_without_touching_the_source() {
        // `crate::move_path` (used by `TrashStore::trash`/`restore_payload`)
        // always tries a plain rename first, which a test running on one
        // real filesystem can never make fail with a genuine EXDEV. Call
        // its cross-device fallback (`crate::copy_verified`) directly
        // instead, exactly as `move_path` would once rename returns an
        // error: this exercises the real cross-device code path itself, not
        // just the rename path it falls back from. Unlike the old fallback
        // this replaces, `copy_verified` never removes `src` itself --
        // that is `move_path`'s own job, done only once the copy is safely
        // swapped into the real destination -- so this only asserts the
        // copy landed correctly and `src` is untouched.
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("payload");
        tokio::fs::write(&src, b"payload-bytes").await.unwrap();
        let dest = dest_dir.path().join("moved-payload");

        crate::copy_verified(&src, &dest).await.unwrap();

        assert_eq!(
            tokio::fs::read(&src).await.unwrap(),
            b"payload-bytes",
            "copy_verified must never touch its source"
        );
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"payload-bytes");
    }

    #[tokio::test]
    async fn cross_device_fallback_copies_a_directory_without_touching_the_source() {
        // Same as above, but shaped like `TrashStore::trash` moving a
        // whole trashed directory's payload subtree.
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("photos");
        tokio::fs::create_dir_all(src.join("sub")).await.unwrap();
        tokio::fs::write(src.join("sub").join("a.jpg"), b"jpeg-bytes")
            .await
            .unwrap();
        let dest = dest_dir.path().join("moved-photos");

        crate::copy_verified(&src, &dest).await.unwrap();

        assert!(
            src.join("sub").join("a.jpg").exists(),
            "copy_verified must never touch its source"
        );
        assert_eq!(
            tokio::fs::read(dest.join("sub").join("a.jpg"))
                .await
                .unwrap(),
            b"jpeg-bytes"
        );
    }

    #[tokio::test]
    async fn trash_and_restore_survive_a_full_move_path_roundtrip() {
        // End-to-end sanity that `TrashStore` still works now that
        // `move_path` is the safe replace primitive rather than a bare
        // rename-or-copy: trash a file, restore it onto a path that
        // already has different real content (the `Overwrite` case), and
        // confirm the restore replaces that content correctly rather than
        // merging or leaving stray files behind.
        let (live, _trash_dir, store) = store(GENEROUS_AGE, GENEROUS_BYTES);
        let live_path = write_live_file(&live, "a.txt", b"trashed content").await;
        let dav_path = DavPath::parse("/a.txt").unwrap();
        let entry = store
            .trash(&live_path, &dav_path, EntryKind::File, 16)
            .await
            .unwrap();

        let restore_dest = live.path().join("a.txt");
        tokio::fs::write(&restore_dest, b"content written after the delete")
            .await
            .unwrap();

        store
            .restore_payload(&entry.id, &restore_dest)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(&restore_dest).await.unwrap(),
            b"trashed content"
        );
    }
}
