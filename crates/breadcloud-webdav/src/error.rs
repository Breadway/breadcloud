use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::StorageError;

/// Map a storage error to the RFC 4918-appropriate HTTP status for the
/// common case. A few handlers (MKCOL, MOVE/COPY) need a different status
/// for `AlreadyExists` than this default and match on the error
/// themselves before falling back here.
pub fn storage_error_response(err: StorageError) -> Response {
    let status = default_status(&err);
    (status, err.to_string()).into_response()
}

fn default_status(err: &StorageError) -> StatusCode {
    match err {
        StorageError::NotFound => StatusCode::NOT_FOUND,
        StorageError::AlreadyExists => StatusCode::CONFLICT,
        StorageError::Conflict(_) => StatusCode::CONFLICT,
        StorageError::IsADirectory => StatusCode::CONFLICT,
        StorageError::NotADirectory => StatusCode::CONFLICT,
        StorageError::InvalidPath(_) => StatusCode::BAD_REQUEST,
        StorageError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// 207-status helper: `http::StatusCode` does not expose a named constant
/// for every WebDAV-registered code across all versions of the crate, so
/// build it from the raw number instead of relying on one.
pub fn multi_status() -> StatusCode {
    StatusCode::from_u16(207).expect("207 is a valid HTTP status code")
}
