use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::StorageError;

/// Map a storage error to the RFC 4918-appropriate HTTP status for the
/// common case. A few handlers (MKCOL, MOVE/COPY) need a different status
/// for `AlreadyExists` than this default and match on the error
/// themselves before falling back here.
pub fn storage_error_response(err: StorageError) -> Response {
    let status = default_status(&err);
    if status.is_server_error() {
        tracing::error!(error = %err, "storage request failed");
        (status, "Internal server error").into_response()
    } else {
        (status, err.to_string()).into_response()
    }
}

fn default_status(err: &StorageError) -> StatusCode {
    match err {
        StorageError::NotFound => StatusCode::NOT_FOUND,
        StorageError::AlreadyExists => StatusCode::CONFLICT,
        StorageError::Conflict(_) => StatusCode::CONFLICT,
        StorageError::IsADirectory => StatusCode::CONFLICT,
        StorageError::NotADirectory => StatusCode::CONFLICT,
        StorageError::LengthMismatch { .. } => StatusCode::PRECONDITION_FAILED,
        StorageError::InvalidPath(_) => StatusCode::BAD_REQUEST,
        StorageError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        // A move's own compensating recovery failed: real content is
        // stranded (not lost) at a path only an operator can safely act
        // on. This is a server-side problem, same bucket as `Io`, not
        // something a client retry can fix.
        StorageError::RecoveryFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        // CLOUD-11: this write would exceed the authenticated user's
        // quota. 507 Insufficient Storage (RFC 4918 11.5) is the
        // WebDAV-correct status for "the request is well-formed, the
        // server just cannot allocate the space" -- distinct from a 4xx
        // client error (the client did nothing wrong) and from 500 (the
        // server isn't broken, the account is just full).
        StorageError::QuotaExceeded { .. } => insufficient_storage(),
    }
}

/// 207-status helper: `http::StatusCode` does not expose a named constant
/// for every WebDAV-registered code across all versions of the crate, so
/// build it from the raw number instead of relying on one.
pub fn multi_status() -> StatusCode {
    StatusCode::from_u16(207).expect("207 is a valid HTTP status code")
}

/// 507-status helper, same reasoning as [`multi_status`]: `StatusCode`
/// has no named constant for it.
pub fn insufficient_storage() -> StatusCode {
    StatusCode::from_u16(507).expect("507 is a valid HTTP status code")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use axum::body::to_bytes;
    use breadcloud_storage::StorageError;

    use super::storage_error_response;

    #[tokio::test]
    async fn internal_storage_errors_hide_paths_from_response_body() {
        let errors = [
            StorageError::Io(std::io::Error::other("failed at /srv/breadcloud/data/file")),
            StorageError::RecoveryFailed {
                primary: Box::new(StorageError::Io(std::io::Error::other("rename failed"))),
                stranded_src: Some(PathBuf::from("/srv/breadcloud/data/.stage-123")),
                stranded_dest: Some(PathBuf::from("/srv/breadcloud/data/.backup-456")),
            },
        ];

        for err in errors {
            let response = storage_error_response(err);
            assert_eq!(
                response.status(),
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            );

            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(body, "Internal server error");
            assert!(!body.contains("/srv/breadcloud"));
        }
    }

    #[tokio::test]
    async fn client_errors_keep_their_existing_message() {
        let response = storage_error_response(StorageError::InvalidPath(
            breadcloud_storage::path::InvalidPath("bad path".to_string()),
        ));
        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "invalid path: bad path"
        );
    }
}
