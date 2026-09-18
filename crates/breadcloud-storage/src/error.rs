use std::path::PathBuf;

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

    /// The assembled size of a chunked upload did not match the client's
    /// declared `OC-Total-Length`. Maps to 412 Precondition Failed: the
    /// client's own precondition on the upload it just finished didn't
    /// hold, so it should not be treated as if it landed correctly.
    #[error("assembled size {actual} does not match declared total length {expected}")]
    LengthMismatch { expected: u64, actual: u64 },

    #[error(transparent)]
    InvalidPath(#[from] InvalidPath),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// [`crate::move_path`]'s own compensating action, run to unwind a
    /// primary step that failed partway (moving the destination's prior
    /// content aside, or swapping the staged replacement into place),
    /// itself failed. This is NOT the same situation as any other variant
    /// above: those all mean "nothing moved, safe to retry or abandon",
    /// while this means real content this crate is responsible for is
    /// currently sitting at `stranded_src` and/or `stranded_dest` instead
    /// of at its rightful location, and has NOT been destroyed but also
    /// has NOT been automatically recovered. A caller must not treat this
    /// the way it treats an ordinary failed move (in particular, must not
    /// go on to purge or otherwise discard anything that assumed the move
    /// either fully succeeded or fully unwound), and should surface it
    /// loudly so an operator can find and manually recover the named
    /// path(s).
    #[error(
        "move recovery failed after a primary failure ({primary}); content was NOT lost but \
         was also NOT restored to its rightful place and needs manual recovery: \
         source content stranded at {stranded_src:?}, destination's original content \
         stranded at {stranded_dest:?}"
    )]
    RecoveryFailed {
        primary: Box<StorageError>,
        /// Set when the source's staged content could not be moved back
        /// to its original location and is instead sitting at this path.
        stranded_src: Option<PathBuf>,
        /// Set when the destination's original content (moved aside
        /// before the swap) could not be moved back into place and is
        /// instead sitting at this path.
        stranded_dest: Option<PathBuf>,
    },

    /// A write would push this user's live storage tree past their
    /// configured quota (CLOUD-11). Maps to 507 Insufficient Storage (RFC
    /// 4918 11.5), the WebDAV-correct status for exactly this condition --
    /// unlike a 4xx client error, the request itself is well-formed, the
    /// server just cannot allocate the space. Callers must check for this
    /// *before* mutating the destination (see
    /// `Storage::check_quota`'s doc comment): nothing on disk changes as a
    /// result of this error, so a rejected write never corrupts or
    /// partially replaces existing content.
    #[error(
        "quota exceeded: this write would bring usage to {used_after} bytes, over the \
         {limit}-byte limit"
    )]
    QuotaExceeded { limit: u64, used_after: u64 },
}
