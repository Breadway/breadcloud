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

pub use error::StorageError;
pub use path::DavPath;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::fs;

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

    /// A quoted etag derived from mtime and size. Cheap enough to compute
    /// on every PROPFIND over a multi-gigabyte tree, and changes whenever
    /// the file is rewritten (mtime bumps) or truncated/extended. It is
    /// not a strong content hash: two writes that land in the same
    /// millisecond with the same length would collide, which is an
    /// acceptable tradeoff against hashing file contents on every listing.
    pub fn etag(&self) -> String {
        let millis = self
            .modified
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("\"{millis:x}-{:x}\"", self.len)
    }
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

/// Filesystem-backed storage, sandboxed to a root directory.
///
/// Cheap to clone (just an `Arc`-free `PathBuf`); callers typically wrap
/// it in an `Arc` once and share it across the whole HTTP server.
#[derive(Debug, Clone)]
pub struct Storage {
    root: PathBuf,
}

impl Storage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Storage { root: root.into() }
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
    pub async fn write_bytes(
        &self,
        path: &DavPath,
        data: &[u8],
    ) -> Result<WriteOutcome, StorageError> {
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

        let tmp_path = parent_fs.join(format!(".breadcloud-tmp-{}", tmp_suffix()));
        fs::write(&tmp_path, data).await?;
        if let Err(e) = fs::rename(&tmp_path, &fs_path).await {
            let _ = fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }

        Ok(if existed {
            WriteOutcome::Overwritten
        } else {
            WriteOutcome::Created
        })
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
        let outcome = storage.write_bytes(&path, b"hi").await.unwrap();
        assert_eq!(outcome, WriteOutcome::Created);
        let meta = storage.metadata(&path).await.unwrap();
        assert_eq!(meta.len, 2);
        assert!(!meta.is_dir());
    }

    #[tokio::test]
    async fn write_existing_file_reports_overwritten() {
        let (_dir, storage) = storage().await;
        let path = DavPath::parse("/hello.txt").unwrap();
        storage.write_bytes(&path, b"hi").await.unwrap();
        let outcome = storage.write_bytes(&path, b"hello there").await.unwrap();
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
}
