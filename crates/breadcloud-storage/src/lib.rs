//! Filesystem-backed storage primitives for breadcloud.
//!
//! This crate has no HTTP or WebDAV knowledge at all: it is a small async
//! wrapper around `tokio::fs` that a) sandboxes every operation to a root
//! directory via [`DavPath`], and b) exposes the outcomes (created vs.
//! overwritten, conflict vs. not-found) that RFC 4918 needs to pick the
//! right status code. Anything built on top (WebDAV today, CalDAV/CardDAV
//! later) talks to storage only through this API.

pub mod error;
pub mod path;
pub mod versions;

pub use error::StorageError;
pub use path::DavPath;
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
    etag_cache: Arc<Mutex<HashMap<PathBuf, CachedEtag>>>,
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Storage {
            root: root.into(),
            etag_cache: Arc::new(Mutex::new(HashMap::new())),
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
    /// file next to it, without touching `dest` itself. Every failure mode
    /// that depends on the state of `dest` or the chunks (an `Overwrite: F`
    /// conflict, an `OC-Total-Length` mismatch, a chunk that vanished mid-
    /// assembly) surfaces here, before any caller-visible content is
    /// replaced.
    ///
    /// This lets a caller know an overwrite will actually happen -- and do
    /// anything that must only happen for a genuine overwrite, such as
    /// taking a version snapshot (CLOUD-4) -- strictly between this call
    /// succeeding and [`Storage::commit_assemble`]. Call [`PreparedAssembly::discard`]
    /// instead of committing if that intervening step fails.
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

        let tmp_path = parent_fs.join(format!(".breadcloud-tmp-{}", tmp_suffix()));
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

    /// Disk usage of the volume backing the storage root.
    pub fn quota(&self) -> Result<Quota, StorageError> {
        let total = fs2::total_space(&self.root)?;
        let available = fs2::available_space(&self.root)?;
        Ok(Quota {
            used_bytes: total.saturating_sub(available),
            available_bytes: available,
        })
    }
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

fn copy_dir_recursive<'a>(
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
}
