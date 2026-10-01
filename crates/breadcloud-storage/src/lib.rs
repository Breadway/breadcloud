//! Filesystem-backed storage primitives for breadcloud.
//!
//! This crate has no HTTP or WebDAV knowledge at all: it is a small async
//! wrapper around `tokio::fs` that a) sandboxes every operation to a root
//! directory via [`DavPath`], and b) exposes the outcomes (created vs.
//! overwritten, conflict vs. not-found) that RFC 4918 needs to pick the
//! right status code. Anything built on top (WebDAV, CalDAV, CardDAV
//! later) talks to storage only through this API. `calendar` (CLOUD-6)
//! and `addressbook` (CLOUD-7) are the exceptions to "sandboxed to a root
//! via `DavPath`": a calendar event or a contact is not addressed by a
//! served-tree path the way a file is, so [`CalendarStore`] and
//! [`AddressbookStore`] each build their own small root-sandboxed layout
//! instead, reusing [`Storage`] itself for each collection's actual
//! `.ics`/`.vcf` files -- see those modules' own doc comments.

pub mod addressbook;
pub mod calendar;
pub mod error;
pub mod path;
pub mod trash;
pub mod versions;

pub use addressbook::{AddressbookInfo, AddressbookStore};
pub use calendar::{CalendarInfo, CalendarStore, ChangeEntry};
pub use error::StorageError;
pub use path::DavPath;
pub use trash::{TrashEntry, TrashStore, DEFAULT_TRASH_MAX_AGE, DEFAULT_TRASH_MAX_TOTAL_BYTES};
pub use versions::{
    VersionInfo, VersionStore, DEFAULT_MAX_VERSIONS_PER_FILE, DEFAULT_MAX_VERSION_SET_BYTES,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Kind of filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// Metadata about a stored resource, enough to answer PROPFIND.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub kind: EntryKind,
    pub len: u64,
    pub modified: std::time::SystemTime,
}

impl Metadata {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    /// A quoted etag derived from mtime and size, at nanosecond precision.
    /// This is what collections get (there is no useful "content" to hash
    /// for a directory), and it is also the cache-validity key behind
    /// [`Storage::etag`]'s content-derived etags for files: cheap to
    /// compute, but not a strong content signature -- two writes to the
    /// same file landing in the same nanosecond with the same length
    /// would collide. See [`Storage::etag`] for the file case, which
    /// hashes content instead and does not have that problem.
    pub fn etag(&self) -> String {
        let nanos = self
            .modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("\"{nanos:x}-{:x}\"", self.len)
    }
}

/// A content-derived etag cached against the (mtime, len) it was computed
/// for, so a PROPFIND over an unchanged tree only ever does hashmap
/// lookups, not rehashing. Invalidated (recomputed) whenever mtime or len
/// changes, which write_bytes/assemble bump on every write.
#[derive(Debug, Clone)]
struct CachedEtag {
    modified: SystemTime,
    len: u64,
    etag: String,
}

/// Disk usage of the filesystem backing the storage root.
///
/// This is filesystem-level, not per-user: until CLOUD-11 adds accounts
/// there is only one user, so "quota" here means "how much room is left
/// on the volume", matching what a single-user WebDAV server can honestly
/// report.
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    pub used_bytes: u64,
    pub available_bytes: u64,
}

/// Outcome of a write-type operation, so the HTTP layer can pick 201
/// Created vs. 204 No Content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Created,
    Overwritten,
}

/// One entry returned by [`Storage::list_dir`] / [`Storage::list_recursive`].
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub path: DavPath,
    pub metadata: Metadata,
}

/// A chunked upload validated and assembled into a temp file by
/// [`Storage::prepare_assemble`], not yet committed onto its destination.
/// Nothing at the destination has been touched: a caller that decides not
/// to proceed should call [`PreparedAssembly::discard`] rather than
/// [`Storage::commit_assemble`], so the temp file does not leak.
pub struct PreparedAssembly {
    dest_fs: PathBuf,
    tmp_path: PathBuf,
    existed: bool,
    total: u64,
    hash: blake3::Hash,
}

impl PreparedAssembly {
    /// Whether the destination already had content that committing this
    /// assembly will overwrite. `false` means this is a first creation,
    /// with nothing for a caller to snapshot before committing.
    pub fn existed(&self) -> bool {
        self.existed
    }

    /// The total assembled size, already validated against
    /// `OC-Total-Length` (if the client sent one) by
    /// `Storage::prepare_assemble`. Used by callers that need to
    /// quota-check (CLOUD-11) a chunked upload's finalize before
    /// committing it -- the content is already fully staged in a temp
    /// file at this point, so its exact size is known without reading
    /// anything back off disk.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Discard this prepared assembly without ever touching its
    /// destination, cleaning up the temp file.
    pub async fn discard(self) {
        let _ = fs::remove_file(&self.tmp_path).await;
    }
}

/// Filesystem-backed storage, sandboxed to a root directory.
///
/// Cheap to clone: the etag cache is behind an `Arc`, so clones share it
/// (callers typically wrap the whole `Storage` in an `Arc` once anyway and
/// share that across the HTTP server, but cloning is safe either way).
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
    /// Where [`Storage::prepare_assemble`] stages a chunked upload's fully
    /// assembled content before it is committed into `root` (CLOUD-11
    /// round 3 rework). A sibling of `root`, not a subdirectory of it --
    /// see [`sibling_staging_root`] for why.
    staging_root: PathBuf,
    etag_cache: Arc<Mutex<HashMap<PathBuf, CachedEtag>>>,
    /// Serializes `check_quota`-then-write sequences for this account
    /// (CLOUD-11): see [`Storage::quota_gate`], which is the only thing
    /// that ever locks this. Each account gets its own `Storage` instance
    /// (its own root, its own clone of this `Arc`), so this never
    /// serializes anything across accounts, only concurrent quota-gated
    /// writes to this one account's tree.
    quota_mutex: Arc<tokio::sync::Mutex<()>>,
    /// Serializes "is this name free, then create it" sequences for this
    /// account (CLOUD-8 rework): see [`Storage::create_gate`]. A separate
    /// lock from `quota_mutex` on purpose -- a caller holding
    /// `create_gate` typically also calls something that itself takes
    /// `quota_gate` for the actual write (e.g.
    /// `breadcloud-notes::handlers::write_note_content`), and
    /// `tokio::sync::Mutex` is not reentrant, so reusing the same lock for
    /// both would deadlock a single request against itself.
    create_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let staging_root = sibling_staging_root(&root);
        Storage {
            root,
            staging_root,
            etag_cache: Arc::new(Mutex::new(HashMap::new())),
            quota_mutex: Arc::new(tokio::sync::Mutex::new(())),
            create_mutex: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a `DavPath` to an absolute filesystem path under the root.
    /// `DavPath` guarantees no `..` segments, so the result can never
    /// escape `root`.
    pub fn resolve(&self, path: &DavPath) -> PathBuf {
        let mut full = self.root.clone();
        for seg in path.segments() {
            full.push(seg);
        }
        full
    }

    pub async fn metadata(&self, path: &DavPath) -> Result<Metadata, StorageError> {
        let fs_path = self.resolve(path);
        let meta = fs::metadata(&fs_path).await.map_err(map_not_found)?;
        Ok(to_metadata(&meta))
    }

    pub async fn exists(&self, path: &DavPath) -> bool {
        fs::metadata(self.resolve(path)).await.is_ok()
    }

    /// Open a file for a streamed GET. Returns `IsADirectory` if `path`
    /// names a collection.
    pub async fn open(&self, path: &DavPath) -> Result<(fs::File, Metadata), StorageError> {
        let fs_path = self.resolve(path);
        let meta = fs::metadata(&fs_path).await.map_err(map_not_found)?;
        if meta.is_dir() {
            return Err(StorageError::IsADirectory);
        }
        let file = fs::File::open(&fs_path).await?;
        Ok((file, to_metadata(&meta)))
    }

    /// List the immediate children of a collection (PROPFIND Depth: 1).
    pub async fn list_dir(&self, path: &DavPath) -> Result<Vec<DirEntry>, StorageError> {
        let fs_path = self.resolve(path);
        let meta = fs::metadata(&fs_path).await.map_err(map_not_found)?;
        if !meta.is_dir() {
            return Err(StorageError::NotADirectory);
        }
        let mut out = Vec::new();
        let mut rd = fs::read_dir(&fs_path).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_meta = entry.metadata().await?;
            out.push(DirEntry {
                path: path.child(&name),
                metadata: to_metadata(&child_meta),
            });
        }
        Ok(out)
    }

    /// List every descendant of a collection, at any depth (PROPFIND
    /// Depth: infinity). Does not include `path` itself.
    pub async fn list_recursive(&self, path: &DavPath) -> Result<Vec<DirEntry>, StorageError> {
        let mut out = Vec::new();
        let mut stack = vec![path.clone()];
        while let Some(dir) = stack.pop() {
            for entry in self.list_dir(&dir).await? {
                if entry.metadata.is_dir() {
                    stack.push(entry.path.clone());
                }
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Write `data` to `path` (PUT). The parent collection must already
    /// exist and be a collection, otherwise this returns `Conflict` (RFC
    /// 4918 9.7.1). Writes go to a temp file in the same directory and
    /// are then renamed into place, so a concurrent GET never observes a
    /// partially-written file.
    ///
    /// Returns the content-derived etag of the written data alongside the
    /// outcome, computed from `data` directly (no extra disk read) and
    /// seeded into the etag cache so the next GET/PROPFIND/HEAD of this
    /// path is a cache hit rather than a rehash.
    pub async fn write_bytes(
        &self,
        path: &DavPath,
        data: &[u8],
    ) -> Result<(WriteOutcome, String), StorageError> {
        let parent_dav = path
            .parent()
            .ok_or_else(|| StorageError::Conflict("cannot write to the storage root".into()))?;
        let parent_fs = self.resolve(&parent_dav);
        let parent_meta = fs::metadata(&parent_fs).await.map_err(|_| {
            StorageError::Conflict(format!("parent collection {parent_dav} does not exist"))
        })?;
        if !parent_meta.is_dir() {
            return Err(StorageError::Conflict(format!(
                "{parent_dav} is not a collection"
            )));
        }

        let fs_path = self.resolve(path);
        let existed = match fs::metadata(&fs_path).await {
            Ok(meta) if meta.is_dir() => return Err(StorageError::IsADirectory),
            Ok(_) => true,
            Err(_) => false,
        };

        let etag = format_etag(blake3::hash(data));

        let tmp_path = parent_fs.join(format!(".breadcloud-tmp-{}", tmp_suffix()));
        fs::write(&tmp_path, data).await?;
        if let Err(e) = fs::rename(&tmp_path, &fs_path).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }

        if let Ok(meta) = fs::metadata(&fs_path).await {
            self.seed_etag_cache(&fs_path, &to_metadata(&meta), etag.clone());
        }

        Ok((
            if existed {
                WriteOutcome::Overwritten
            } else {
                WriteOutcome::Created
            },
            etag,
        ))
    }

    /// Read a file's full content into memory. Used by version
    /// snapshotting (CLOUD-4), which needs the previous content of a file
    /// immediately before an overwrite replaces it. Not used for serving
    /// GET (`handlers::get` streams instead: target files can be large,
    /// and GET is on the hot path for concurrent downloads in a way a
    /// pre-overwrite snapshot is not).
    pub async fn read_bytes(&self, path: &DavPath) -> Result<Vec<u8>, StorageError> {
        let (mut file, meta) = self.open(path).await?;
        let mut buf = Vec::with_capacity(meta.len as usize);
        file.read_to_end(&mut buf).await?;
        Ok(buf)
    }

    /// The etag RFC 4918 clients should see for `path`: content-derived
    /// (cached, keyed on mtime+len) for a file, the cheaper mtime/len
    /// signature for a collection (there is no meaningful "content" to
    /// hash for a directory; see [`Metadata::etag`]).
    ///
    /// Consistent across PROPFIND, GET, HEAD and PUT/MOVE/COPY responses:
    /// every one of those handlers goes through this method (or reuses a
    /// value it returned) rather than deriving its own etag.
    pub async fn etag(&self, path: &DavPath) -> Result<String, StorageError> {
        let fs_path = self.resolve(path);
        let meta = fs::metadata(&fs_path).await.map_err(map_not_found)?;
        self.content_etag(&fs_path, &to_metadata(&meta)).await
    }

    /// Same as [`Storage::etag`], but for a caller that already has this
    /// path's `Metadata` in hand (from `metadata`, `list_dir`, or
    /// `list_recursive`) and wants to avoid a redundant stat -- notably
    /// PROPFIND, which fetches metadata for a whole directory listing and
    /// would otherwise stat every entry twice.
    pub async fn etag_for(&self, path: &DavPath, meta: &Metadata) -> Result<String, StorageError> {
        let fs_path = self.resolve(path);
        self.content_etag(&fs_path, meta).await
    }

    /// Content-derived etag for a file already known to exist, reusing a
    /// `Metadata` the caller already fetched. Checks the cache first;
    /// only reads and hashes the file when the cache is cold or stale
    /// (mtime/len changed since the cached hash was computed, e.g. a
    /// write that happened outside `write_bytes`/`assemble`, or the first
    /// access after a server restart with an empty cache).
    async fn content_etag(&self, fs_path: &Path, meta: &Metadata) -> Result<String, StorageError> {
        if meta.is_dir() {
            return Ok(meta.etag());
        }
        {
            let cache = self.etag_cache.lock().expect("etag cache lock poisoned");
            if let Some(cached) = cache.get(fs_path) {
                if cached.modified == meta.modified && cached.len == meta.len {
                    return Ok(cached.etag.clone());
                }
            }
        }

        let hash = hash_file(fs_path).await?;
        let etag = format_etag(hash);
        self.seed_etag_cache(fs_path, meta, etag.clone());
        Ok(etag)
    }

    fn seed_etag_cache(&self, fs_path: &Path, meta: &Metadata, etag: String) {
        let mut cache = self.etag_cache.lock().expect("etag cache lock poisoned");
        cache.insert(
            fs_path.to_path_buf(),
            CachedEtag {
                modified: meta.modified,
                len: meta.len,
                etag,
            },
        );
    }

    /// Assemble `chunk_files` (in the order given -- the caller is
    /// responsible for sorting them into upload order) into `dest`,
    /// streaming each one through rather than buffering the whole result
    /// in memory. Used to finalize a chunked (NG chunking v2) upload: the
    /// chunks live in a separate staging `Storage`, so this takes plain
    /// filesystem paths for them rather than `DavPath`s in `self`.
    ///
    /// `expected_total`, when given (from the client's `OC-Total-Length`
    /// header), is checked against the assembled size before the result
    /// is renamed into place; a mismatch leaves `dest` untouched and
    /// returns `StorageError::LengthMismatch` so the caller can 412 and
    /// let the client retry rather than silently accepting a truncated or
    /// overlong file.
    /// Validate `chunk_files` against `dest` and assemble them into a temp
    /// file, without touching `dest` itself. Every failure mode that
    /// depends on the state of `dest` or the chunks (an `Overwrite: F`
    /// conflict, an `OC-Total-Length` mismatch, a chunk that vanished mid-
    /// assembly) surfaces here, before any caller-visible content is
    /// replaced.
    ///
    /// This lets a caller know an overwrite will actually happen -- and do
    /// anything that must only happen for a genuine overwrite, such as
    /// taking a version snapshot (CLOUD-4) -- strictly between this call
    /// succeeding and [`Storage::commit_assemble`]. Call [`PreparedAssembly::discard`]
    /// instead of committing if that intervening step fails.
    ///
    /// The assembled content is staged in `self.staging_root` (a sibling
    /// of `self.root`, see [`sibling_staging_root`]), not inside `self.root`
    /// next to `dest`'s parent. CLOUD-11 round 3: staging it inside `root`
    /// meant a quota check that ran between this call and
    /// [`Storage::commit_assemble`] (necessarily after this call, since
    /// the assembled size is only known once assembly is done) would walk
    /// right over the still-staged temp file via `used_bytes`'s recursive
    /// tally, counting the about-to-be-committed bytes once as "already on
    /// disk" and a second time as the incoming write, and wrongly reject
    /// uploads at roughly half their real allowed size. Staging outside
    /// `root` entirely removes that double count structurally, with no
    /// exclusion-list logic for `tally` to get wrong or fall out of sync
    /// with a future staging-file naming change.
    pub async fn prepare_assemble(
        &self,
        dest: &DavPath,
        chunk_files: &[PathBuf],
        overwrite: bool,
        expected_total: Option<u64>,
    ) -> Result<PreparedAssembly, StorageError> {
        let parent_dav = dest
            .parent()
            .ok_or_else(|| StorageError::Conflict("cannot write to the storage root".into()))?;
        let parent_fs = self.resolve(&parent_dav);
        let parent_meta = fs::metadata(&parent_fs).await.map_err(|_| {
            StorageError::Conflict(format!("parent collection {parent_dav} does not exist"))
        })?;
        if !parent_meta.is_dir() {
            return Err(StorageError::Conflict(format!(
                "{parent_dav} is not a collection"
            )));
        }

        let dest_fs = self.resolve(dest);
        let existed = match fs::metadata(&dest_fs).await {
            Ok(meta) if meta.is_dir() => return Err(StorageError::IsADirectory),
            Ok(_) => true,
            Err(_) => false,
        };
        if existed && !overwrite {
            return Err(StorageError::AlreadyExists);
        }

        fs::create_dir_all(&self.staging_root).await?;
        let tmp_path = self
            .staging_root
            .join(format!(".breadcloud-tmp-{}", tmp_suffix()));
        let result = assemble_into(&tmp_path, chunk_files, expected_total).await;
        let (total, hash) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path).await;
                return Err(e);
            }
        };

        Ok(PreparedAssembly {
            dest_fs,
            tmp_path,
            existed,
            total,
            hash,
        })
    }

    /// Atomically replace `prepared`'s destination with the assembled
    /// content. The only failure mode left at this point is the rename
    /// itself; everything that depends on chunk or destination state was
    /// already validated by [`Storage::prepare_assemble`].
    pub async fn commit_assemble(
        &self,
        prepared: PreparedAssembly,
    ) -> Result<(WriteOutcome, u64, String), StorageError> {
        let PreparedAssembly {
            dest_fs,
            tmp_path,
            existed,
            total,
            hash,
        } = prepared;

        if let Err(e) = fs::rename(&tmp_path, &dest_fs).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }

        let etag = format_etag(hash);
        if let Ok(meta) = fs::metadata(&dest_fs).await {
            self.seed_etag_cache(&dest_fs, &to_metadata(&meta), etag.clone());
        }

        Ok((
            if existed {
                WriteOutcome::Overwritten
            } else {
                WriteOutcome::Created
            },
            total,
            etag,
        ))
    }

    /// Convenience wrapper for callers that have no intervening step
    /// between validation and commit (nothing that must only happen once
    /// the overwrite is known to succeed). Equivalent to calling
    /// [`Storage::prepare_assemble`] then immediately
    /// [`Storage::commit_assemble`].
    pub async fn assemble(
        &self,
        dest: &DavPath,
        chunk_files: &[PathBuf],
        overwrite: bool,
        expected_total: Option<u64>,
    ) -> Result<(WriteOutcome, u64, String), StorageError> {
        let prepared = self
            .prepare_assemble(dest, chunk_files, overwrite, expected_total)
            .await?;
        self.commit_assemble(prepared).await
    }

    /// Create a new, empty collection (MKCOL). The parent must already
    /// exist; `path` itself must not.
    pub async fn mkcol(&self, path: &DavPath) -> Result<(), StorageError> {
        let parent_dav = match path.parent() {
            Some(p) => p,
            None => return Err(StorageError::Conflict("root already exists".into())),
        };
        let parent_fs = self.resolve(&parent_dav);
        let parent_meta = fs::metadata(&parent_fs).await.map_err(|_| {
            StorageError::Conflict(format!("parent collection {parent_dav} does not exist"))
        })?;
        if !parent_meta.is_dir() {
            return Err(StorageError::Conflict(format!(
                "{parent_dav} is not a collection"
            )));
        }

        let fs_path = self.resolve(path);
        if fs::metadata(&fs_path).await.is_ok() {
            return Err(StorageError::AlreadyExists);
        }
        fs::create_dir(&fs_path).await?;
        Ok(())
    }

    /// Delete a file, or a collection and everything under it.
    pub async fn delete(&self, path: &DavPath) -> Result<(), StorageError> {
        let fs_path = self.resolve(path);
        let meta = fs::metadata(&fs_path).await.map_err(map_not_found)?;
        if meta.is_dir() {
            fs::remove_dir_all(&fs_path).await?;
        } else {
            fs::remove_file(&fs_path).await?;
        }
        Ok(())
    }

    /// Move (rename) `from` to `to`. `overwrite` mirrors the RFC 4918
    /// Overwrite header: when false and `to` already exists, this returns
    /// `AlreadyExists` (the caller maps that to 412 Precondition Failed).
    pub async fn move_to(
        &self,
        from: &DavPath,
        to: &DavPath,
        overwrite: bool,
    ) -> Result<WriteOutcome, StorageError> {
        self.relocate(from, to, overwrite, false).await
    }

    /// Copy `from` to `to`, recursively if `from` is a collection.
    /// `overwrite` mirrors the RFC 4918 Overwrite header.
    pub async fn copy_to(
        &self,
        from: &DavPath,
        to: &DavPath,
        overwrite: bool,
    ) -> Result<WriteOutcome, StorageError> {
        self.relocate(from, to, overwrite, true).await
    }

    /// Count the source and check the destination while holding the same
    /// quota lock as PUT and chunk finalization. A source size read before
    /// this lock could become stale when a concurrent PUT grows the source.
    pub async fn copy_to_with_quota(
        &self,
        from: &DavPath,
        to: &DavPath,
        overwrite: bool,
        limit_bytes: u64,
    ) -> Result<WriteOutcome, StorageError> {
        let _guard = self.quota_mutex.lock().await;
        let metadata = self.metadata(from).await?;
        let source_bytes = if metadata.is_dir() {
            self.list_recursive(from)
                .await?
                .iter()
                .filter(|entry| !entry.metadata.is_dir())
                .map(|entry| entry.metadata.len)
                .sum()
        } else {
            metadata.len
        };
        self.check_quota(limit_bytes, to, source_bytes).await?;
        self.copy_to(from, to, overwrite).await
    }

    async fn relocate(
        &self,
        from: &DavPath,
        to: &DavPath,
        overwrite: bool,
        copy: bool,
    ) -> Result<WriteOutcome, StorageError> {
        if from == to {
            return Err(StorageError::Conflict(
                "source and destination are the same".into(),
            ));
        }
        if from.is_ancestor_of(to) {
            return Err(StorageError::Conflict(
                "cannot move or copy a collection into itself".into(),
            ));
        }

        let from_fs = self.resolve(from);
        let from_meta = fs::metadata(&from_fs).await.map_err(map_not_found)?;

        let to_parent = to
            .parent()
            .ok_or_else(|| StorageError::Conflict("cannot write to the storage root".into()))?;
        let to_parent_fs = self.resolve(&to_parent);
        let to_parent_meta = fs::metadata(&to_parent_fs).await.map_err(|_| {
            StorageError::Conflict(format!("destination collection {to_parent} does not exist"))
        })?;
        if !to_parent_meta.is_dir() {
            return Err(StorageError::Conflict(format!(
                "{to_parent} is not a collection"
            )));
        }

        let to_fs = self.resolve(to);
        let existing = fs::metadata(&to_fs).await.ok();
        let existed = existing.is_some();
        if let Some(existing_meta) = existing {
            if !overwrite {
                return Err(StorageError::AlreadyExists);
            }
            if existing_meta.is_dir() {
                fs::remove_dir_all(&to_fs).await?;
            } else {
                fs::remove_file(&to_fs).await?;
            }
        }

        if copy {
            if from_meta.is_dir() {
                copy_dir_recursive(&from_fs, &to_fs).await?;
            } else {
                fs::copy(&from_fs, &to_fs).await?;
            }
        } else {
            fs::rename(&from_fs, &to_fs).await?;
        }

        Ok(if existed {
            WriteOutcome::Overwritten
        } else {
            WriteOutcome::Created
        })
    }

    /// Disk usage of the volume backing the storage root. Filesystem-level,
    /// not user-level: this is what a `Storage` with no configured
    /// per-user quota limit reports (a single-account deployment, or a
    /// standalone test/dev instance -- see `Storage::quota_report`, which
    /// is what CLOUD-11's multi-user handlers actually call).
    pub fn quota(&self) -> Result<Quota, StorageError> {
        let total = fs2::total_space(&self.root)?;
        let available = fs2::available_space(&self.root)?;
        Ok(Quota {
            used_bytes: total.saturating_sub(available),
            available_bytes: available,
        })
    }

    /// Total bytes of every file under this root, recursively. This is
    /// "how much of this user's own quota has this Storage instance's
    /// tree consumed" (CLOUD-11): since each account gets its own
    /// root-sandboxed `Storage`, walking the whole tree is walking exactly
    /// that account's live data, nothing else. Deliberately does not
    /// include this instance's own version history or trash (those are
    /// separate `VersionStore`/`TrashStore` roots with their own,
    /// independent retention caps already bounding their size -- see the
    /// README's "Multi-user accounts" section for why quota is scoped to
    /// the live served tree, not those too).
    ///
    /// Reuses the same recursive walk [`copy_verified`]'s size check
    /// already needed (`tally`), rather than a second directory-walking
    /// implementation: this and that call are answering the same
    /// question ("how many bytes live under this path"), just for
    /// different roots.
    ///
    /// A chunked upload's in-flight assembly (`Storage::prepare_assemble`)
    /// never appears in this walk: it stages content in
    /// `self.staging_root`, a sibling of `root` rather than anything
    /// under it (CLOUD-11 round 3 rework -- see [`sibling_staging_root`]).
    /// This call needs no awareness of that staging area at all, which is
    /// the point: nothing here has to stay in sync with wherever
    /// `prepare_assemble` happens to put its temp files.
    pub async fn used_bytes(&self) -> Result<u64, StorageError> {
        let (_count, bytes) = tally(&self.root).await?;
        Ok(bytes)
    }

    /// The quota this Storage instance should report on a PROPFIND
    /// (CLOUD-11): given a per-user limit, `used_bytes` against it rather
    /// than the whole-volume figure [`Storage::quota`] reports. `None`
    /// (no configured limit -- a single-account or standalone/test
    /// instance) falls back to that whole-volume figure exactly as
    /// before CLOUD-11, so a deployment that never opts into per-user
    /// quotas keeps reporting the same thing it always did.
    pub async fn quota_report(&self, limit_bytes: Option<u64>) -> Result<Quota, StorageError> {
        match limit_bytes {
            Some(limit) => {
                let used = self.used_bytes().await?;
                Ok(Quota {
                    used_bytes: used,
                    available_bytes: limit.saturating_sub(used),
                })
            }
            None => self.quota(),
        }
    }

    /// Whether writing `incoming_len` bytes to `path` would push this
    /// account's live tree over `limit_bytes` (CLOUD-11). Nets out
    /// `path`'s own current size first when it already exists -- an
    /// overwrite replaces those bytes, it does not add to them on top of
    /// what is already counted in `used_bytes`, so a same-size or smaller
    /// overwrite of an existing file is never incorrectly rejected once
    /// a user is already right at their limit.
    ///
    /// Callers must run this, and act on `Err(StorageError::QuotaExceeded)`,
    /// strictly *before* any write that would mutate `path` or commit new
    /// content (`write_bytes`, `commit_assemble`): everything this method
    /// touches is read-only (a metadata stat and a recursive size walk),
    /// so a rejection here is guaranteed to leave `path` and every other
    /// resource in this tree completely untouched -- no partial write, no
    /// corrupted existing content, exactly the property a quota-rejected
    /// write must have.
    ///
    /// This alone is not safe under concurrency: two callers can both call
    /// this, both see room, and both then write, together blowing past
    /// the limit (a plain TOCTOU race -- see CLOUD-11's rework history).
    /// A caller that is actually about to write, rather than just report a
    /// number (e.g. a PROPFIND's `quota-used-bytes`), must go through
    /// [`Storage::quota_gate`] instead, which wraps this exact check in
    /// the serialization needed to close that window.
    pub async fn check_quota(
        &self,
        limit_bytes: u64,
        path: &DavPath,
        incoming_len: u64,
    ) -> Result<QuotaCheck, StorageError> {
        let existing_len = match self.metadata(path).await {
            Ok(meta) if !meta.is_dir() => meta.len,
            _ => 0,
        };
        let used_before = self.used_bytes().await?;
        let used_after = used_before
            .saturating_sub(existing_len)
            .saturating_add(incoming_len);
        if used_after > limit_bytes {
            return Err(StorageError::QuotaExceeded {
                limit: limit_bytes,
                used_after,
                used_before,
            });
        }
        Ok(QuotaCheck {
            limit_bytes,
            used_before,
            used_after,
        })
    }

    /// The safe entry point for a quota-gated write (CLOUD-11): closes the
    /// TOCTOU window a bare [`Storage::check_quota`] call leaves open by
    /// acquiring this account's own async mutex first, then running
    /// `check_quota` while holding it, and handing the caller back a
    /// [`QuotaGuard`] that keeps the lock held until the caller drops it.
    /// A second concurrent call against this same `Storage` instance
    /// blocks on the mutex until the first caller's guard is dropped --
    /// which callers must not do until the write `check_quota` just
    /// approved has actually landed (or been abandoned) -- so the second
    /// call's own `check_quota` always sees the first write's bytes
    /// already counted in `used_bytes`, and the two can never both
    /// observe "room available" for overlapping capacity.
    ///
    /// On `Err` (quota exceeded, or `check_quota`'s own I/O failure) the
    /// lock is released immediately: the guard is only ever constructed
    /// on success, so a rejected caller never has to remember to drop
    /// anything.
    ///
    /// This is the one implementation both the plain PUT path
    /// (`breadcloud-webdav::handlers::put`) and the chunked-upload
    /// finalize path (`breadcloud-webdav::handlers::uploads::finalize`)
    /// go through, rather than two independent ad-hoc locks that could
    /// drift out of sync with each other -- see the `move_path` docs
    /// above for what that pattern has cost this codebase before
    /// (CLOUD-5's five rework rounds over duplicated cross-root move
    /// logic).
    ///
    /// Different accounts never contend on this: each account gets its
    /// own `Storage` instance (its own root, its own `quota_mutex`), so
    /// concurrent writes to *different* accounts still proceed fully in
    /// parallel. Only concurrent quota-gated writes to the *same* account
    /// serialize.
    pub async fn quota_gate(
        &self,
        limit_bytes: u64,
        path: &DavPath,
        incoming_len: u64,
    ) -> Result<QuotaGuard, StorageError> {
        let guard = self.quota_mutex.clone().lock_owned().await;
        let check = self.check_quota(limit_bytes, path, incoming_len).await?;
        Ok(QuotaGuard { lock: guard, check })
    }

    /// Serializes a whole "check whether some name is free, then create
    /// it" sequence for this account (CLOUD-8 rework), the same TOCTOU fix
    /// as [`Storage::quota_gate`] applied to a different race: a caller
    /// like `breadcloud-notes::handlers::create` picks a dedup-suffixed
    /// filename by probing for an unused one, then writes it; without a
    /// lock held across both steps, two concurrent callers can each probe
    /// while the other's write has not landed yet, both pick the same
    /// "free" name, and the second write silently destroys the first
    /// caller's content (confirmed live against the real server, see
    /// CLOUD-8's review history).
    ///
    /// Held across the whole probe-then-write sequence, not just the
    /// write, so the second caller's own probe only starts once the first
    /// caller's file already exists on disk. Per-account granularity: each
    /// account gets its own `Storage` instance and its own clone of this
    /// `Arc`, so this never serializes create sequences across accounts.
    pub async fn create_gate(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.create_mutex.clone().lock_owned().await
    }
}

/// The before/after usage a successful [`Storage::check_quota`] or
/// [`Storage::quota_gate`] call computed (CLOUD-11), exposed so a caller
/// can detect a threshold crossing (CLOUD-13's `bread.cloud.quota.warning`,
/// see `breadcloud-events::crossed_threshold`) without a second,
/// redundant usage walk.
#[derive(Debug, Clone, Copy)]
pub struct QuotaCheck {
    pub limit_bytes: u64,
    pub used_before: u64,
    pub used_after: u64,
}

/// RAII guard returned by [`Storage::quota_gate`] on success. Holds this
/// account's quota mutex; the caller must keep it alive (bind it to a
/// named variable, not `_`) until the write it was granted for has
/// completed or been abandoned, then let it drop (explicitly, or by
/// falling out of scope). Dropping it any earlier re-opens the exact
/// TOCTOU window `quota_gate` exists to close.
pub struct QuotaGuard {
    #[allow(dead_code)]
    lock: tokio::sync::OwnedMutexGuard<()>,
    /// The usage this call's `check_quota` computed. See [`QuotaCheck`].
    pub check: QuotaCheck,
}

fn to_metadata(meta: &std::fs::Metadata) -> Metadata {
    Metadata {
        kind: if meta.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        len: meta.len(),
        modified: meta.modified().unwrap_or(UNIX_EPOCH),
    }
}

fn map_not_found(e: std::io::Error) -> StorageError {
    if e.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound
    } else {
        StorageError::Io(e)
    }
}

fn format_etag(hash: blake3::Hash) -> String {
    // 32 hex chars (128 bits) is plenty of collision resistance for an
    // etag and keeps the header short; blake3's full digest is 64 hex
    // chars.
    format!("\"{}\"", &hash.to_hex()[..32])
}

/// Hash a file's full content. Used when the etag cache is cold (first
/// access after a restart, or a file written outside `write_bytes`).
async fn hash_file(fs_path: &Path) -> Result<blake3::Hash, StorageError> {
    let mut file = fs::File::open(fs_path).await.map_err(map_not_found)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Stream-concatenate `chunk_files` into `tmp_path` in order, hashing as
/// it goes. Returns the total bytes written and their combined hash.
/// Leaves `tmp_path`'s state up to the caller on error (they remove it).
async fn assemble_into(
    tmp_path: &Path,
    chunk_files: &[PathBuf],
    expected_total: Option<u64>,
) -> Result<(u64, blake3::Hash), StorageError> {
    let mut out = fs::File::create(tmp_path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];

    for chunk_path in chunk_files {
        let mut chunk = fs::File::open(chunk_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::Conflict(format!(
                    "chunk {} vanished before assembly",
                    chunk_path.display()
                ))
            } else {
                StorageError::Io(e)
            }
        })?;
        loop {
            let n = chunk.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).await?;
            hasher.update(&buf[..n]);
            total += n as u64;
        }
    }
    out.flush().await?;

    if let Some(expected) = expected_total {
        if expected != total {
            return Err(StorageError::LengthMismatch {
                expected,
                actual: total,
            });
        }
    }

    Ok((total, hasher.finalize()))
}

fn tmp_suffix() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", nanos, std::process::id())
}

/// Where [`Storage::prepare_assemble`] stages a chunked upload's fully
/// assembled content before [`Storage::commit_assemble`] lands it in the
/// live tree (CLOUD-11 round 3 rework).
///
/// Deliberately a *sibling* of `root` (same parent directory, different
/// path), not a subdirectory of it:
///
/// - Structurally outside `used_bytes`'s recursive walk of `root`, so an
///   in-flight assembly can never be counted as "already on disk" quota
///   usage. This needs no exclusion list inside `tally` (which also backs
///   `copy_verified`'s cross-root copy check, CLOUD-5, and stays
///   unaware of this reserved name entirely) that a future rename or
///   staging-name change could silently stop matching.
/// - Unreachable through [`Storage::resolve`], which only ever builds
///   paths under `root`: no `DavPath` a client sends, however it is
///   percent-encoded or cased, can ever land here, unlike a same-tree
///   dotfile/dot-directory naming convention a client could in principle
///   collide with by naming a real resource the same thing.
/// - On the same filesystem as `root` (same parent directory), so
///   `commit_assemble`'s `fs::rename` into the live tree stays a single
///   atomic filesystem-local rename. This staging area is intentionally
///   *not* `AppState::uploads`'s already-separate chunk-staging root:
///   that root is independently configured (`BREADCLOUD_UPLOADS_ROOT`,
///   defaulting to the system temp dir) and not guaranteed to share a
///   filesystem with `root`. Renaming across a filesystem boundary either
///   fails outright (`EXDEV`) or forces a non-atomic copy-then-delete,
///   which would reopen exactly the "`dest` is never observed partially
///   written" guarantee `commit_assemble`'s docs promise.
fn sibling_staging_root(root: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(root.file_name().unwrap_or_default());
    name.push("-assembly-staging");
    root.with_file_name(name)
}

/// # Cross-root move safety
///
/// [`move_path`] is the one primitive behind every move that crosses one of
/// this codebase's independently-configurable storage roots: `TrashStore`
/// moving a resource's payload into and out of a trash entry, and
/// `VersionStore` moving a file's version history into and out of one
/// alongside it. Three earlier rounds of fixes in this area (see the CLOUD-5
/// issue history) each patched one partial-failure path here, only to have
/// the fix itself introduce a new one: a rename-then-copy-on-EXDEV fallback
/// that didn't reorder a caller's own two dependent moves safely; a rollback
/// added to fix that, which was itself just another unconditionally
/// destructive move. The common root cause was always the same: treating
/// "does the destination already have content" as the signal for whether
/// it's safe to discard, when that signal cannot tell a genuine pre-existing
/// file apart from a partial leftover of a previous, half-failed attempt at
/// this exact move.
///
/// [`move_path`] sidesteps that distinction instead of trying to make it.
/// Nothing at `dest` is ever destroyed until `src`'s content is fully
/// staged, on `dest`'s own filesystem, and confirmed complete; nothing at
/// `src` is ever removed until that staged content is durably swapped into
/// `dest`. Concretely:
///
/// 1. `src`'s content is moved into a staging path beside `dest` (same
///    directory, so guaranteed the same filesystem as `dest`). A plain
///    rename is tried first -- free when `src` and `dest` happen to share a
///    filesystem too -- falling back to a recursive copy, verified complete
///    (same file count and total bytes as `src`) before anything downstream
///    trusts it. A failure here leaves both `src` and `dest` completely
///    untouched.
/// 2. Whatever currently occupies `dest`, if anything, is renamed aside
///    (same filesystem, atomic) rather than deleted. It is not touched
///    again until step 3 has committed.
/// 3. The staged content is renamed into `dest`. Same filesystem as both the
///    staging path and the old-content backup, so this is a single atomic
///    rename.
/// 4. Only now -- new content durably live at `dest` -- are the old `dest`
///    content (from step 2) and the original `src` cleaned up, both
///    best-effort: neither failing is a correctness problem, only litter.
///
/// A failure at step 2 or 3 unwinds back to the pre-call state (`src`
/// restored if step 1 moved it by rename rather than copy; `dest`'s original
/// content renamed back if it had been moved aside), so callers can treat
/// an ordinary `Err` from this function as "nothing moved", exactly as
/// before this redesign. Because nothing is ever destroyed before its
/// replacement is confirmed safely in place, the whole operation is
/// idempotent: a caller that retries a failed higher-level operation (a
/// soft-delete, a restore, a rollback of either) from scratch never
/// compounds a first failure into a second one, and never needs bespoke
/// per-call rollback logic layered on top -- retrying just re-runs this
/// same safe replace, which is a no-op if `dest` already holds the right
/// content.
///
/// That unwind is itself just more filesystem mutation, and it can fail
/// too (the same class of fault that broke the primary step -- a
/// permission change, a disk remount, an I/O hiccup -- can just as easily
/// hit the compensating rename a moment later). When it does, this
/// function returns [`StorageError::RecoveryFailed`] instead of the
/// primary error, naming exactly where the content that could not be put
/// back is sitting. Callers must check for that variant specifically and
/// must not treat it as "nothing moved": unlike an ordinary `Err`, it means
/// this function's own safety net also failed, and something upstream
/// (ideally a human, per the loud `tracing::error!` this logs) needs to
/// look at the named path(s) before anything else touches `src`, `dest`,
/// or whatever container (a trash entry, in this codebase) they live
/// under.
///
/// Shared by [`crate::trash::TrashStore`] (moving a resource's payload
/// into/out of a trash entry) and [`crate::versions::VersionStore`]
/// (moving a file's version history into/out of a trash entry, including
/// the rollback move a failed restore uses to undo a version-history move
/// that already succeeded): both a trash root and a versions root are
/// independently configurable and can each land on a different volume than
/// the served tree, and any of these calls can be the one that lands on the
/// live side or the trash side of an in-progress, possibly-retried
/// operation, so all of them need this same safety, not just the common
/// case.
pub(crate) async fn move_path(src: &Path, dest: &Path) -> Result<(), StorageError> {
    let dest_parent = dest
        .parent()
        .ok_or_else(|| StorageError::Conflict("destination has no parent directory".into()))?;
    fs::create_dir_all(dest_parent).await?;

    let staging = dest_parent.join(format!(".breadcloud-stage-{}", tmp_suffix()));

    // Step 1: get a complete copy of `src`'s content onto `dest`'s
    // filesystem, at `staging`, without touching `src` or `dest` yet.
    let staged_via_rename = fs::rename(src, &staging).await.is_ok();
    if !staged_via_rename {
        if let Err(e) = copy_verified(src, &staging).await {
            remove_any(&staging).await;
            return Err(e);
        }
    }

    // Step 2: move whatever is at `dest` aside rather than deleting it --
    // it might be real content this call must not lose, or nothing at
    // all. Either way it is not touched again until step 3 commits.
    let backup = dest_parent.join(format!(".breadcloud-backup-{}", tmp_suffix()));
    let had_backup = match fs::rename(dest, &backup).await {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(
                unwind_after_failure(src, &staging, staged_via_rename, None, e.into()).await,
            );
        }
    };

    // Step 3: the atomic swap. `staging`, `backup` and `dest` all share a
    // parent directory, so this is a single same-filesystem rename.
    if let Err(e) = fs::rename(&staging, dest).await {
        let backup_restore = if had_backup {
            Some((backup.as_path(), dest))
        } else {
            None
        };
        return Err(unwind_after_failure(
            src,
            &staging,
            staged_via_rename,
            backup_restore,
            e.into(),
        )
        .await);
    }

    // Step 4: new content is durably live at `dest`. Clean up, best
    // effort -- nothing below can turn this success back into a failure.
    if had_backup {
        remove_any(&backup).await;
    }
    if !staged_via_rename {
        remove_any(src).await;
    }
    Ok(())
}

/// Undo step 1 of [`move_path`] on a later failure, so callers can keep
/// relying on "`src` is untouched on `Err`" regardless of which path step 1
/// took internally. The `Result` is load-bearing, not litter: when
/// `staged_via_rename` is true, `staging` holds `src`'s *only* copy of the
/// content, so a failure here means that content is stranded rather than
/// restored, and the caller (`unwind_after_failure`) must surface that
/// distinctly rather than discard it via `let _ =`.
async fn restore_staged_src(
    src: &Path,
    staging: &Path,
    staged_via_rename: bool,
) -> std::io::Result<()> {
    if staged_via_rename {
        // `staging` holds `src`'s only copy (a plain rename doesn't
        // duplicate); put it back.
        fs::rename(staging, src).await
    } else {
        // `src` was never touched; only the copy at `staging` needs
        // cleanup. Failing to remove it is litter, not data loss -- the
        // one real copy is still at `src`, untouched.
        remove_any(staging).await;
        Ok(())
    }
}

/// Unwind [`move_path`]'s step 2 or step 3 back to the pre-call state after
/// one of them fails: restore `dest`'s original content (if `backup_restore`
/// says it was moved aside to a backup path), then restore `src`'s content
/// (via [`restore_staged_src`]). `primary` is the error that triggered the
/// unwind in the first place.
///
/// Returns `primary` unchanged if both compensating actions succeed (or
/// there was nothing to undo for one of them) -- exactly the "nothing
/// moved" contract callers of [`move_path`] rely on. If either compensating
/// action itself fails, returns [`StorageError::RecoveryFailed`] instead,
/// naming precisely where the content that could not be put back is
/// sitting, and logs it at `error!` (not `warn!`): this is a materially
/// different, worse outcome than the primary failure alone, and must never
/// be folded into the same "safe to retry" `Err` a caller already knows how
/// to handle.
async fn unwind_after_failure(
    src: &Path,
    staging: &Path,
    staged_via_rename: bool,
    backup_restore: Option<(&Path, &Path)>,
    primary: StorageError,
) -> StorageError {
    let stranded_dest = match backup_restore {
        Some((backup, dest)) => match fs::rename(backup, dest).await {
            Ok(()) => None,
            Err(_) => Some(backup.to_path_buf()),
        },
        None => None,
    };

    let stranded_src = match restore_staged_src(src, staging, staged_via_rename).await {
        Ok(()) => None,
        Err(_) => Some(staging.to_path_buf()),
    };

    if stranded_src.is_none() && stranded_dest.is_none() {
        return primary;
    }

    tracing::error!(
        "move_path recovery failed after a primary failure ({primary}); \
         stranded_src={stranded_src:?} stranded_dest={stranded_dest:?}; this content has \
         NOT been lost but needs manual recovery, do not touch these paths automatically"
    );
    StorageError::RecoveryFailed {
        primary: Box::new(primary),
        stranded_src,
        stranded_dest,
    }
}

/// Remove a file or directory tree, ignoring the result: used for
/// best-effort cleanup where failing to remove something is litter, not a
/// correctness problem.
async fn remove_any(path: &Path) {
    match fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(path).await;
        }
        Ok(_) => {
            let _ = fs::remove_file(path).await;
        }
        Err(_) => {}
    }
}

/// Recursively copy `src` to `dest`, then verify the copy landed whole
/// (same file count and total bytes as `src`) before returning. This is
/// [`move_path`]'s cross-filesystem fallback: exercised whenever a plain
/// rename cannot move a trashed directory or a version-history subtree in
/// one step, most likely because `src` and `dest` are on different
/// filesystems (a rename can never cross those, e.g. `EXDEV`). Split out
/// from [`move_path`] so it can be exercised directly by a test without
/// needing two real filesystems to provoke a genuine rename failure.
///
/// Never touches `src`: removing it once its content is safely elsewhere is
/// [`move_path`]'s job, not this function's, since [`move_path`] only knows
/// it is safe to do so once the copy verified here has also been swapped
/// into `dest` in place of whatever was there before.
///
/// On any failure -- the copy itself, or the verification -- any partial
/// output at `dest` is removed before returning, so a caller never has to
/// distinguish "nothing was written" from "something incomplete was
/// written": either this fully succeeds and `dest` matches `src` exactly,
/// or it fails and `dest` has nothing new.
pub(crate) async fn copy_verified(src: &Path, dest: &Path) -> Result<(), StorageError> {
    match copy_verified_inner(src, dest).await {
        Ok(()) => Ok(()),
        Err(e) => {
            remove_any(dest).await;
            Err(e)
        }
    }
}

async fn copy_verified_inner(src: &Path, dest: &Path) -> Result<(), StorageError> {
    let meta = fs::metadata(src).await.map_err(map_not_found)?;
    if meta.is_dir() {
        copy_dir_recursive(src, dest).await?;
    } else {
        fs::copy(src, dest).await?;
    }

    let (src_count, src_bytes) = tally(src).await?;
    let (dest_count, dest_bytes) = tally(dest).await?;
    if src_count != dest_count || src_bytes != dest_bytes {
        return Err(StorageError::Io(std::io::Error::other(format!(
            "copy verification failed: source had {src_count} file(s) totalling \
             {src_bytes} bytes, staged copy had {dest_count} file(s) totalling \
             {dest_bytes} bytes"
        ))));
    }
    Ok(())
}

/// A recursive async function's boxed, pinned future -- needed because
/// [`tally`] calls itself across an `.await` and Rust cannot express an
/// infinitely-sized recursive `async fn` any other way.
type TallyFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(u64, u64), StorageError>> + Send + 'a>,
>;

/// Total file count and byte size of `path`, recursively if it is a
/// directory (one file is `(1, len)`). Used only by [`copy_verified`] to
/// confirm a copy landed completely; cheap (a second directory walk, no
/// rehashing) relative to the copy it is checking.
fn tally(path: &Path) -> TallyFuture<'_> {
    Box::pin(async move {
        let meta = fs::metadata(path).await?;
        if !meta.is_dir() {
            return Ok((1, meta.len()));
        }
        let mut count = 0u64;
        let mut bytes = 0u64;
        let mut rd = fs::read_dir(path).await?;
        while let Some(entry) = rd.next_entry().await? {
            let child_meta = entry.metadata().await?;
            if child_meta.is_dir() {
                let (c, b) = tally(&entry.path()).await?;
                count += c;
                bytes += b;
            } else {
                count += 1;
                bytes += child_meta.len();
            }
        }
        Ok((count, bytes))
    })
}

/// Recursively copy a directory tree. `pub(crate)` so [`copy_verified`] (the
/// verified cross-filesystem copy behind [`move_path`], shared by
/// [`crate::trash`] and [`crate::versions`]) can reuse it when a plain
/// rename cannot move a trashed directory or a version-history subtree into
/// place in one step. Also reused directly by [`Storage::relocate`] for an
/// ordinary same-root `COPY`, which has no cross-filesystem concern and no
/// need for verification (there is no separate `src` to protect: on
/// failure, whatever this leaves at `to_fs` is just replaced by the next
/// attempt).
pub(crate) fn copy_dir_recursive<'a>(
    from: &'a Path,
    to: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        fs::create_dir_all(to).await?;
        let mut rd = fs::read_dir(from).await?;
        while let Some(entry) = rd.next_entry().await? {
            let meta = entry.metadata().await?;
            let src = entry.path();
            let dst = to.join(entry.file_name());
            if meta.is_dir() {
                copy_dir_recursive(&src, &dst).await?;
            } else {
                fs::copy(&src, &dst).await?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn storage() -> (TempDir, Storage) {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path());
        (dir, storage)
    }

    #[tokio::test]
    async fn write_new_file_reports_created() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/hello.txt").unwrap();
        let (outcome, etag) = storage.write_bytes(&path, b"hi").await.unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert!(!etag.is_empty());
        let meta = storage.metadata(&path).await.unwrap();
        assert_eq!(meta.len, 2);
        assert!(!meta.is_dir());
    }

    #[tokio::test]
    async fn write_existing_file_reports_overwritten() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/hello.txt").unwrap();
        storage.write_bytes(&path, b"hi").await.unwrap();
        let (outcome, _etag) = storage.write_bytes(&path, b"hello there").await.unwrap();
        assert_eq!(outcome, WriteOutcome::Overwritten);
        let meta = storage.metadata(&path).await.unwrap();
        assert_eq!(meta.len, 11);
    }

    #[tokio::test]
    async fn write_with_missing_parent_is_conflict() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/missing/hello.txt").unwrap();
        let err = storage.write_bytes(&path, b"hi").await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn write_over_a_directory_is_rejected() {
        let (_dir, storage) = storage().await;
        let dir_path = DavPath::parse("/adir").unwrap();
        storage.mkcol(&dir_path).await.unwrap();
        let err = storage.write_bytes(&dir_path, b"hi").await.unwrap_err();
        assert!(matches!(err, StorageError::IsADirectory));
    }

    #[tokio::test]
    async fn mkcol_then_list_dir() {
        let (_dir, storage) = storage().await;
        let dir_path = DavPath::parse("/photos").unwrap();
        storage.mkcol(&dir_path).await.unwrap();
        storage
            .write_bytes(&dir_path.child("a.jpg"), b"jpeg-bytes")
            .await
            .unwrap();
        storage.mkcol(&dir_path.child("sub")).await.unwrap();

        let entries = storage.list_dir(&dir_path).await.unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().filter_map(|e| e.path.name()).collect();
        assert!(names.contains(&"a.jpg"));
        assert!(names.contains(&"sub"));
    }

    #[tokio::test]
    async fn mkcol_twice_fails() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/photos").unwrap();
        storage.mkcol(&path).await.unwrap();
        let err = storage.mkcol(&path).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists));
    }

    #[tokio::test]
    async fn mkcol_missing_parent_is_conflict() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a/b").unwrap();
        let err = storage.mkcol(&path).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn list_recursive_walks_nested_collections() {
        let (_dir, storage) = storage().await;
        storage.mkcol(&DavPath::parse("/a").unwrap()).await.unwrap();
        storage
            .mkcol(&DavPath::parse("/a/b").unwrap())
            .await
            .unwrap();
        storage
            .write_bytes(&DavPath::parse("/a/b/file.txt").unwrap(), b"x")
            .await
            .unwrap();

        let entries = storage.list_recursive(&DavPath::root()).await.unwrap();
        let paths: Vec<String> = entries.iter().map(|e| e.path.to_request_path()).collect();
        assert!(paths.contains(&"/a".to_string()));
        assert!(paths.contains(&"/a/b".to_string()));
        assert!(paths.contains(&"/a/b/file.txt".to_string()));
    }

    #[tokio::test]
    async fn delete_file_and_directory() {
        let (_dir, storage) = storage().await;
        let file = DavPath::parse("/f.txt").unwrap();
        storage.write_bytes(&file, b"x").await.unwrap();
        storage.delete(&file).await.unwrap();
        assert!(!storage.exists(&file).await);

        let dir_path = DavPath::parse("/d").unwrap();
        storage.mkcol(&dir_path).await.unwrap();
        storage
            .write_bytes(&dir_path.child("child.txt"), b"x")
            .await
            .unwrap();
        storage.delete(&dir_path).await.unwrap();
        assert!(!storage.exists(&dir_path).await);
    }

    #[tokio::test]
    async fn delete_missing_is_not_found() {
        let (_dir, storage) = storage().await;
        let err = storage
            .delete(&DavPath::parse("/nope.txt").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound));
    }

    #[tokio::test]
    async fn move_renames_and_reports_created() {
        let (_dir, storage) = storage().await;
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        storage.write_bytes(&from, b"content").await.unwrap();

        let outcome = storage.move_to(&from, &to, false).await.unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert!(!storage.exists(&from).await);
        assert!(storage.exists(&to).await);
    }

    #[tokio::test]
    async fn move_onto_existing_without_overwrite_fails() {
        let (_dir, storage) = storage().await;
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        storage.write_bytes(&from, b"content").await.unwrap();
        storage.write_bytes(&to, b"other").await.unwrap();

        let err = storage.move_to(&from, &to, false).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists));
    }

    #[tokio::test]
    async fn move_onto_existing_with_overwrite_replaces() {
        let (_dir, storage) = storage().await;
        let from = DavPath::parse("/a.txt").unwrap();
        let to = DavPath::parse("/b.txt").unwrap();
        storage.write_bytes(&from, b"content").await.unwrap();
        storage.write_bytes(&to, b"other").await.unwrap();

        let outcome = storage.move_to(&from, &to, true).await.unwrap();
        assert_eq!(outcome, WriteOutcome::Overwritten);
        let meta = storage.metadata(&to).await.unwrap();
        assert_eq!(meta.len, 7);
    }

    #[tokio::test]
    async fn move_collection_into_itself_is_conflict() {
        let (_dir, storage) = storage().await;
        let parent = DavPath::parse("/a").unwrap();
        storage.mkcol(&parent).await.unwrap();
        let child = parent.child("b");

        let err = storage.move_to(&parent, &child, false).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn copy_collection_into_itself_is_conflict() {
        let (_dir, storage) = storage().await;
        let parent = DavPath::parse("/a").unwrap();
        storage.mkcol(&parent).await.unwrap();
        let child = parent.child("sub");

        let err = storage.copy_to(&parent, &child, false).await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    #[tokio::test]
    async fn copy_directory_recursively() {
        let (_dir, storage) = storage().await;
        let src = DavPath::parse("/src").unwrap();
        storage.mkcol(&src).await.unwrap();
        storage.mkcol(&src.child("nested")).await.unwrap();
        storage
            .write_bytes(&src.child("nested").child("file.txt"), b"payload")
            .await
            .unwrap();

        let dst = DavPath::parse("/dst").unwrap();
        storage.copy_to(&src, &dst, false).await.unwrap();

        assert!(storage.exists(&src.child("nested").child("file.txt")).await);
        assert!(storage.exists(&dst.child("nested").child("file.txt")).await);
        let meta = storage
            .metadata(&dst.child("nested").child("file.txt"))
            .await
            .unwrap();
        assert_eq!(meta.len, 7);
    }

    #[tokio::test]
    async fn quota_reports_nonzero_available_space() {
        let (_dir, storage) = storage().await;
        let quota = storage.quota().unwrap();
        assert!(quota.available_bytes > 0);
    }

    #[tokio::test]
    async fn etag_is_content_derived_not_just_mtime() {
        let (_dir, storage) = storage().await;
        let a = DavPath::parse("/a.txt").unwrap();
        let b = DavPath::parse("/b.txt").unwrap();
        // Same length, different content, written back to back (likely
        // the same nanosecond on a fast test machine): a pure mtime+len
        // etag would be prone to colliding here, a content etag never
        // does.
        storage.write_bytes(&a, b"aaaaaaaa").await.unwrap();
        storage.write_bytes(&b, b"bbbbbbbb").await.unwrap();
        let etag_a = storage.etag(&a).await.unwrap();
        let etag_b = storage.etag(&b).await.unwrap();
        assert_ne!(etag_a, etag_b);
    }

    #[tokio::test]
    async fn etag_is_stable_across_rereads() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a.txt").unwrap();
        storage.write_bytes(&path, b"stable content").await.unwrap();
        let first = storage.etag(&path).await.unwrap();
        let second = storage.etag(&path).await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn etag_changes_when_content_is_overwritten() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a.txt").unwrap();
        storage.write_bytes(&path, b"version one").await.unwrap();
        let before = storage.etag(&path).await.unwrap();
        storage.write_bytes(&path, b"version two").await.unwrap();
        let after = storage.etag(&path).await.unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn etag_matches_the_one_returned_by_write_bytes() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a.txt").unwrap();
        let (_outcome, written_etag) = storage.write_bytes(&path, b"payload").await.unwrap();
        let read_etag = storage.etag(&path).await.unwrap();
        assert_eq!(written_etag, read_etag);
    }

    #[tokio::test]
    async fn etag_recomputes_when_written_outside_write_bytes() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a.txt").unwrap();
        storage.write_bytes(&path, b"first").await.unwrap();
        let first_etag = storage.etag(&path).await.unwrap();

        // Simulate a write that bypassed write_bytes (e.g. a file dropped
        // directly into the served tree): the cache still has the old
        // (mtime, len) keyed to the old hash, so this must detect the
        // mismatch and rehash rather than serving a stale etag.
        let fs_path = storage.resolve(&path);
        tokio::fs::write(&fs_path, b"replaced-outside-storage")
            .await
            .unwrap();

        let second_etag = storage.etag(&path).await.unwrap();
        assert_ne!(first_etag, second_etag);
    }

    #[tokio::test]
    async fn assemble_concatenates_chunks_in_given_order() {
        let (dir, storage) = storage().await;
        let chunks_dir = dir.path().join("chunks");
        tokio::fs::create_dir_all(&chunks_dir).await.unwrap();
        let c0 = chunks_dir.join("0");
        let c1 = chunks_dir.join("1");
        tokio::fs::write(&c0, b"hello, ").await.unwrap();
        tokio::fs::write(&c1, b"world!").await.unwrap();

        let dest = DavPath::parse("/assembled.txt").unwrap();
        let (outcome, total, etag) = storage
            .assemble(&dest, &[c0, c1], false, None)
            .await
            .unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        assert_eq!(total, 13);
        assert!(!etag.is_empty());

        let meta = storage.metadata(&dest).await.unwrap();
        assert_eq!(meta.len, 13);
        assert_eq!(storage.etag(&dest).await.unwrap(), etag);
    }

    #[tokio::test]
    async fn assemble_rejects_mismatched_total_length() {
        let (dir, storage) = storage().await;
        let chunk = dir.path().join("chunk0");
        tokio::fs::write(&chunk, b"only-ten-b").await.unwrap();

        let dest = DavPath::parse("/assembled.txt").unwrap();
        let err = storage
            .assemble(&dest, &[chunk], false, Some(999))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::LengthMismatch { .. }));
        assert!(!storage.exists(&dest).await);
    }

    #[tokio::test]
    async fn assemble_onto_existing_without_overwrite_fails() {
        let (dir, storage) = storage().await;
        let dest = DavPath::parse("/dest.txt").unwrap();
        storage.write_bytes(&dest, b"already here").await.unwrap();

        let chunk = dir.path().join("chunk0");
        tokio::fs::write(&chunk, b"new content").await.unwrap();

        let err = storage
            .assemble(&dest, &[chunk], false, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists));
    }

    #[tokio::test]
    async fn read_bytes_returns_the_full_content() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/a.txt").unwrap();
        storage.write_bytes(&path, b"some content").await.unwrap();
        let bytes = storage.read_bytes(&path).await.unwrap();
        assert_eq!(bytes, b"some content");
    }

    #[tokio::test]
    async fn read_bytes_missing_file_is_not_found() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/nope.txt").unwrap();
        let err = storage.read_bytes(&path).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound));
    }

    #[tokio::test]
    async fn assemble_missing_parent_is_conflict() {
        let (dir, storage) = storage().await;
        let chunk = dir.path().join("chunk0");
        tokio::fs::write(&chunk, b"x").await.unwrap();

        let dest = DavPath::parse("/missing/dest.txt").unwrap();
        let err = storage
            .assemble(&dest, &[chunk], false, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)));
    }

    // `move_path` itself: the safe replace primitive behind every
    // cross-storage-root move in this codebase (see its doc comment for
    // the full safety argument). These exercise it directly, independent
    // of `TrashStore`/`VersionStore`, covering the properties every caller
    // relies on: a fresh destination is just a move, an occupied
    // destination is replaced only once the replacement is confirmed
    // whole, and any failure before that point leaves both sides exactly
    // as they were.

    #[tokio::test]
    async fn move_path_onto_a_fresh_destination_just_moves_it() {
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("payload");
        tokio::fs::write(&src, b"content").await.unwrap();
        let dest = dest_dir.path().join("nested").join("payload");

        move_path(&src, &dest).await.unwrap();

        assert!(!src.exists());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"content");
    }

    #[tokio::test]
    async fn move_path_onto_an_occupied_destination_replaces_it_without_merging() {
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("payload");
        tokio::fs::write(&src, b"new content").await.unwrap();
        let dest = dest_dir.path().join("payload");
        tokio::fs::write(&dest, b"old content").await.unwrap();

        move_path(&src, &dest).await.unwrap();

        assert!(!src.exists());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"new content");
        // The old content's backup/staging litter must not survive a
        // successful move.
        let leftovers: Vec<_> = std::fs::read_dir(dest_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("payload")]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn move_path_forced_failure_before_the_swap_leaves_both_sides_untouched() {
        // Force the earliest possible failure -- staging `src`'s content
        // beside `dest` -- by making `dest`'s parent read-only. Neither
        // `src` nor `dest`'s real, pre-existing content may be touched:
        // this is the property every caller (`TrashStore`, `VersionStore`)
        // depends on to treat any `Err` from this function as "nothing
        // moved".
        use std::os::unix::fs::PermissionsExt;

        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let src = src_dir.path().join("payload");
        tokio::fs::write(&src, b"the only copy of this content")
            .await
            .unwrap();
        let dest_parent = dest_dir.path().join("entry");
        tokio::fs::create_dir_all(&dest_parent).await.unwrap();
        let dest = dest_parent.join("payload");
        tokio::fs::write(&dest, b"real content already at the destination")
            .await
            .unwrap();

        let mut perms = std::fs::metadata(&dest_parent).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(&dest_parent, perms.clone()).unwrap();

        let err = move_path(&src, &dest).await.unwrap_err();
        assert!(matches!(err, StorageError::Io(_)));
        assert_eq!(
            tokio::fs::read(&src).await.unwrap(),
            b"the only copy of this content",
            "a failure before the swap must never touch src"
        );

        perms.set_mode(0o755);
        std::fs::set_permissions(&dest_parent, perms).unwrap();
        assert_eq!(
            tokio::fs::read(&dest).await.unwrap(),
            b"real content already at the destination",
            "a failure before the swap must never touch dest's real content"
        );

        // Retrying with permissions fixed must fully succeed.
        move_path(&src, &dest).await.unwrap();
        assert!(!src.exists());
        assert_eq!(
            tokio::fs::read(&dest).await.unwrap(),
            b"the only copy of this content"
        );
    }

    // Round 5 (CLOUD-5): `move_path`'s own step 2/3 error recovery
    // (`restore_staged_src`, and the step 3 backup-restore) used to discard
    // the result of its own compensating `fs::rename` via `let _ = ...` and
    // then return the *primary* error regardless of whether the
    // compensation actually worked -- so a failed recovery looked
    // identical to the safe "nothing moved" case, while the content it was
    // supposed to restore was actually stranded. These tests exercise the
    // two recovery helpers directly (the same level the reviewer's own
    // repro used): `move_path` shares one `dest_parent` between `staging`,
    // `backup` and `dest`, so once that directory is writable enough for
    // step 1 to stage content there, steps 2 and 3 need the exact same
    // permission to run -- there is no static, non-racing filesystem state
    // that fails step 2 or step 3 without also failing step 1 first. The
    // recovery helpers are the one place this failure mode can be forced
    // deterministically without instrumenting a race into `move_path`
    // itself, which is exactly why the reviewer's own reproduction was
    // built at this level too.

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_staged_src_reports_failure_instead_of_silently_stranding_content() {
        // The reviewer's exact round-5 repro: `staging` holds `src`'s only
        // copy of some content (as it does whenever `move_path` staged it
        // via a plain rename), and the rename-back that is supposed to
        // undo that is made to fail by taking away write permission on
        // `src`'s parent directory at the moment of the call. Before this
        // fix, the old `let _ = fs::rename(staging, src).await;` discarded
        // that failure and returned as if the undo had succeeded, leaving
        // the content stranded at `staging` with nothing pointing back to
        // it.
        use std::os::unix::fs::PermissionsExt;

        let src_parent = TempDir::new().unwrap();
        let staging_dir = TempDir::new().unwrap();
        let src = src_parent.path().join("payload");
        let staging = staging_dir.path().join(".breadcloud-stage-test");
        tokio::fs::write(&staging, b"the only copy of this content")
            .await
            .unwrap();

        let mut perms = std::fs::metadata(src_parent.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(src_parent.path(), perms.clone()).unwrap();

        let result = restore_staged_src(&src, &staging, true).await;

        perms.set_mode(0o755);
        std::fs::set_permissions(src_parent.path(), perms).unwrap();

        assert!(
            result.is_err(),
            "a failed rename-back must be reported, not silently swallowed"
        );
        assert!(
            !src.exists(),
            "src's parent was unwritable, so the rename-back could not possibly have landed"
        );
        assert_eq!(
            tokio::fs::read(&staging).await.unwrap(),
            b"the only copy of this content",
            "the content must still be findable at the staging path, not lost"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwind_after_failure_reports_recovery_failed_when_the_src_restore_fails() {
        // `unwind_after_failure` is what step 2 and step 3's error
        // branches both call. When its own attempt to restore `src` fails
        // (the same fault as above), it must return
        // `StorageError::RecoveryFailed` naming `staging` as where the
        // content actually is, not the original primary error -- a caller
        // must never be able to mistake this for the ordinary "nothing
        // moved" case.
        use std::os::unix::fs::PermissionsExt;

        let src_parent = TempDir::new().unwrap();
        let staging_dir = TempDir::new().unwrap();
        let src = src_parent.path().join("payload");
        let staging = staging_dir.path().join(".breadcloud-stage-test");
        tokio::fs::write(&staging, b"only copy").await.unwrap();

        let mut perms = std::fs::metadata(src_parent.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(src_parent.path(), perms.clone()).unwrap();

        let primary = StorageError::Io(std::io::Error::other("primary failure"));
        let result = unwind_after_failure(&src, &staging, true, None, primary).await;

        perms.set_mode(0o755);
        std::fs::set_permissions(src_parent.path(), perms).unwrap();

        match result {
            StorageError::RecoveryFailed {
                stranded_src,
                stranded_dest,
                ..
            } => {
                assert_eq!(stranded_src, Some(staging.clone()));
                assert_eq!(stranded_dest, None);
            }
            other => panic!("expected RecoveryFailed, got {other:?}"),
        }
        assert_eq!(
            tokio::fs::read(&staging).await.unwrap(),
            b"only copy",
            "the content must still be findable at the staging path, not lost"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwind_after_failure_reports_recovery_failed_when_the_backup_restore_fails() {
        // Same idea, for the step 3 backup-restore: when moving `dest`'s
        // original content back from `backup` fails, `unwind_after_failure`
        // must report `RecoveryFailed` naming `backup` as where that
        // content actually is, rather than returning the primary error as
        // if `dest` had been safely put back.
        use std::os::unix::fs::PermissionsExt;

        let dest_parent = TempDir::new().unwrap();
        let src_parent = TempDir::new().unwrap();
        let dest = dest_parent.path().join("payload");
        let backup = dest_parent.path().join(".breadcloud-backup-test");
        let src = src_parent.path().join("payload");
        tokio::fs::write(&backup, b"dest's original content")
            .await
            .unwrap();
        tokio::fs::write(&src, b"src content, restored independently")
            .await
            .unwrap();

        let mut perms = std::fs::metadata(dest_parent.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dest_parent.path(), perms.clone()).unwrap();

        let primary = StorageError::Io(std::io::Error::other("primary failure"));
        // `staged_via_rename: false` here so the src-restore side of this
        // call is a no-op (nothing to undo there), isolating the assertion
        // to the backup-restore failure this test is about.
        let result = unwind_after_failure(
            &src,
            Path::new("/unused"),
            false,
            Some((&backup, &dest)),
            primary,
        )
        .await;

        perms.set_mode(0o755);
        std::fs::set_permissions(dest_parent.path(), perms).unwrap();

        match result {
            StorageError::RecoveryFailed {
                stranded_src,
                stranded_dest,
                ..
            } => {
                assert_eq!(stranded_src, None);
                assert_eq!(stranded_dest, Some(backup.clone()));
            }
            other => panic!("expected RecoveryFailed, got {other:?}"),
        }
        assert_eq!(
            tokio::fs::read(&backup).await.unwrap(),
            b"dest's original content",
            "the content must still be findable at the backup path, not lost"
        );
        assert!(
            !dest.exists(),
            "dest was never actually restored, so it must not appear to exist"
        );
    }

    #[tokio::test]
    async fn used_bytes_sums_every_file_recursively() {
        let (_dir, storage) = storage().await;
        storage
            .write_bytes(&DavPath::parse("/a.txt").unwrap(), b"12345")
            .await
            .unwrap();
        storage
            .mkcol(&DavPath::parse("/sub").unwrap())
            .await
            .unwrap();
        storage
            .write_bytes(&DavPath::parse("/sub/b.txt").unwrap(), b"1234567890")
            .await
            .unwrap();

        assert_eq!(storage.used_bytes().await.unwrap(), 15);
    }

    #[tokio::test]
    async fn check_quota_rejects_a_write_over_the_limit() {
        let (_dir, storage) = storage().await;
        storage
            .write_bytes(&DavPath::parse("/existing.txt").unwrap(), b"12345")
            .await
            .unwrap();

        let err = storage
            .check_quota(10, &DavPath::parse("/new.txt").unwrap(), 10)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::QuotaExceeded { .. }));
        // The rejected write must never have touched disk.
        assert!(!storage.exists(&DavPath::parse("/new.txt").unwrap()).await);
    }

    #[tokio::test]
    async fn check_quota_allows_a_write_within_the_limit() {
        let (_dir, storage) = storage().await;
        storage
            .check_quota(10, &DavPath::parse("/new.txt").unwrap(), 10)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn check_quota_nets_out_the_target_s_own_existing_size_on_overwrite() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/f.txt").unwrap();
        storage.write_bytes(&path, b"1234567890").await.unwrap(); // 10 bytes, right at the limit

        // Overwriting the same file with content the same size must not
        // be rejected as if the new bytes were added on top of the old
        // ones still counted in `used_bytes`.
        storage.check_quota(10, &path, 10).await.unwrap();

        // But a genuinely larger overwrite still gets rejected.
        let err = storage.check_quota(10, &path, 11).await.unwrap_err();
        assert!(matches!(err, StorageError::QuotaExceeded { .. }));
    }

    #[tokio::test]
    async fn copy_rechecks_source_size_after_a_concurrent_growth() {
        use std::future::Future;
        use std::task::Poll;

        let (_dir, storage) = storage().await;
        let source = DavPath::parse("/source.bin").unwrap();
        let destination = DavPath::parse("/copy.bin").unwrap();
        storage.write_bytes(&source, &[b'a'; 10]).await.unwrap();

        // Model an admitted PUT still holding its quota reservation. COPY
        // must wait before reading the source size, then reject the larger
        // source without creating a destination.
        let put_guard = storage.quota_gate(30, &source, 20).await.unwrap();
        let mut copy = Box::pin(storage.copy_to_with_quota(&source, &destination, true, 30));
        std::future::poll_fn(|cx| {
            assert!(matches!(copy.as_mut().poll(cx), Poll::Pending));
            Poll::Ready(())
        })
        .await;
        storage.write_bytes(&source, &[b'b'; 20]).await.unwrap();
        drop(put_guard);

        assert!(matches!(
            copy.await,
            Err(StorageError::QuotaExceeded { .. })
        ));
        assert_eq!(storage.used_bytes().await.unwrap(), 20);
        assert!(!storage.exists(&destination).await);
    }

    /// The bug this whole CLOUD-11 rework closes, exercised directly
    /// against the serialization primitive rather than through HTTP: a
    /// bare `check_quota` call has no way to stop two concurrent callers
    /// each individually well under a limit from both proceeding and
    /// together blowing past it. `quota_gate` must close that window by
    /// serializing the check-then-write sequence per `Storage` instance.
    ///
    /// Uses a real multi-threaded runtime and holds each guard across a
    /// short sleep (standing in for the write itself taking a moment) so
    /// the two tasks are genuinely overlapping in time, not just racing
    /// the test executor's scheduling order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn quota_gate_prevents_two_concurrent_writes_from_together_exceeding_the_limit() {
        let (_dir, storage) = storage().await;
        let limit = 1000u64;

        let storage_a = storage.clone();
        let storage_b = storage.clone();

        let task_a = tokio::spawn(async move {
            let path = DavPath::parse("/a.bin").unwrap();
            match storage_a.quota_gate(limit, &path, 700).await {
                Ok(guard) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    storage_a.write_bytes(&path, &vec![0u8; 700]).await.unwrap();
                    drop(guard);
                    true
                }
                Err(_) => false,
            }
        });
        let task_b = tokio::spawn(async move {
            let path = DavPath::parse("/b.bin").unwrap();
            match storage_b.quota_gate(limit, &path, 700).await {
                Ok(guard) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    storage_b.write_bytes(&path, &vec![0u8; 700]).await.unwrap();
                    drop(guard);
                    true
                }
                Err(_) => false,
            }
        });

        let (a_ok, b_ok) = tokio::join!(task_a, task_b);
        let a_ok = a_ok.unwrap();
        let b_ok = b_ok.unwrap();

        // Each write (700 bytes) is individually well under the
        // 1000-byte limit; only their sum (1400) exceeds it. Both
        // succeeding is exactly the race this test guards against.
        assert_ne!(
            a_ok, b_ok,
            "exactly one of the two concurrent 700-byte writes against a 1000-byte quota \
             must be accepted, the other rejected"
        );

        let used = storage.used_bytes().await.unwrap();
        assert!(
            used <= limit,
            "final on-disk usage {used} must never exceed the {limit}-byte quota"
        );
    }

    /// CLOUD-11 round 3: a real bug distinct from the round-2 TOCTOU race
    /// above, found with zero concurrency involved. `prepare_assemble`
    /// stages a chunked upload's fully assembled content into a temp file
    /// before its caller can run `quota_gate` (the exact size is only
    /// known once assembly finishes). If that temp file were staged
    /// anywhere `used_bytes`'s recursive walk of `self.root` could see
    /// it, `quota_gate`'s `check_quota` would count it once as
    /// "already on disk" and a second time as the incoming write,
    /// wrongly rejecting an upload at roughly half its real allowed
    /// size. Sized just over half of the quota so the bug, if
    /// reintroduced, trips this assertion rather than passing by luck.
    #[tokio::test]
    async fn prepare_assemble_then_quota_gate_does_not_double_count_the_staged_temp_file() {
        let (_dir, storage) = storage().await;
        // A separate TempDir, not a subdirectory of `storage`'s own root:
        // in production, chunks live in `AppState::uploads`, a wholly
        // separate `Storage` instance from the account's main tree
        // (`AppState::storage`) that `used_bytes` never walks. Sourcing
        // the chunk from inside `storage`'s own root here would make this
        // test's "incoming" bytes indistinguishable from genuine
        // pre-existing usage, defeating the point of isolating the bug
        // this test guards against.
        let chunks_dir = TempDir::new().unwrap();
        let chunk = chunks_dir.path().join("0");
        tokio::fs::write(&chunk, vec![7u8; 600]).await.unwrap();

        let dest = DavPath::parse("/big.bin").unwrap();
        let prepared = storage
            .prepare_assemble(&dest, &[chunk], false, None)
            .await
            .unwrap();
        assert_eq!(prepared.total(), 600);

        let limit = 1000u64;
        let guard = match storage.quota_gate(limit, &dest, prepared.total()).await {
            Ok(guard) => guard,
            Err(err) => panic!(
                "a 600-byte chunked upload against a 1000-byte quota on a completely \
                 empty account must not be rejected: {err:?}"
            ),
        };
        storage.commit_assemble(prepared).await.unwrap();
        drop(guard);

        let used = storage.used_bytes().await.unwrap();
        assert_eq!(
            used, 600,
            "final usage must be exactly the assembled size, not double-counted"
        );
    }

    /// The fix for the double-count above must not overcorrect: a
    /// chunked upload that is genuinely over quota must still be
    /// rejected, and the rejection must leave nothing committed.
    #[tokio::test]
    async fn prepare_assemble_then_quota_gate_still_rejects_a_genuinely_over_quota_upload() {
        let (_dir, storage) = storage().await;
        // Same reasoning as the test above: chunk source lives outside
        // `storage`'s own root, matching `AppState::uploads` in production.
        let chunks_dir = TempDir::new().unwrap();
        let chunk = chunks_dir.path().join("0");
        tokio::fs::write(&chunk, vec![7u8; 1100]).await.unwrap();

        let dest = DavPath::parse("/too-big.bin").unwrap();
        let prepared = storage
            .prepare_assemble(&dest, &[chunk], false, None)
            .await
            .unwrap();
        assert_eq!(prepared.total(), 1100);

        let limit = 1000u64;
        let result = storage.quota_gate(limit, &dest, prepared.total()).await;
        match result {
            Err(err) => assert!(matches!(err, StorageError::QuotaExceeded { .. })),
            Ok(_) => panic!("a 1100-byte upload against a 1000-byte quota must be rejected"),
        }
        prepared.discard().await;

        assert!(!storage.exists(&dest).await);
        assert_eq!(storage.used_bytes().await.unwrap(), 0);
    }
}
