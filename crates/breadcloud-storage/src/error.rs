use crate::path::InvalidPath;

/// Errors from the storage layer. The WebDAV layer maps each variant to
/// the RFC 4918-appropriate HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,

    #[error("already exists")]
    AlreadyExists,

    /// The requested operation cannot complete because of the state of
    /// some other resource (missing parent collection, parent that is not
    /// a collection, moving a collection into itself). Maps to 409
    /// Conflict.
    #[error("conflict: {0}")]
    Conflict(String),

    #[error("is a directory")]
    IsADirectory,

    #[error("is not a directory")]
    NotADirectory,

    #[error(transparent)]
    InvalidPath(#[from] InvalidPath),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
