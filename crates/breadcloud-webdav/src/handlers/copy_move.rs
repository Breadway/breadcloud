use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError, WriteOutcome};
use percent_encoding::percent_decode_str;

use crate::error::storage_error_response;
use crate::state::AppState;

/// Shared MOVE/COPY handling: both take a `Destination` header and an
/// optional `Overwrite` header per RFC 4918 9.9 / 9.8.
pub async fn handle(
    state: &AppState,
    source: &DavPath,
    headers: &HeaderMap,
    copy: bool,
) -> Response {
    if source.is_root() {
        return (
            StatusCode::FORBIDDEN,
            "cannot move or copy the storage root",
        )
            .into_response();
    }

    let destination = match extract_destination_path(headers) {
        Ok(p) => p,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    // RFC 4918 10.6: Overwrite defaults to T (true) when the header is
    // absent; only an explicit "F" means "fail if the destination exists".
    let overwrite = headers
        .get("Overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|v| !v.eq_ignore_ascii_case("F"))
        .unwrap_or(true);

    let result = if copy {
        state.storage.copy_to(source, &destination, overwrite).await
    } else {
        state.storage.move_to(source, &destination, overwrite).await
    };

    match result {
        Ok(outcome) => {
            let status = match outcome {
                WriteOutcome::Created => StatusCode::CREATED,
                WriteOutcome::Overwritten => StatusCode::NO_CONTENT,
            };
            // Best-effort: report the resulting resource's etag the same
            // way PUT does, so a client that relies on the response
            // (rather than a follow-up PROPFIND) to learn the new etag
            // stays in sync. Not fatal if this races with something else
            // touching the destination immediately after.
            match state.storage.etag(&destination).await {
                Ok(etag) => Response::builder()
                    .status(status)
                    .header("ETag", etag)
                    .body(Body::empty())
                    .unwrap(),
                Err(_) => status.into_response(),
            }
        }
        // RFC 4918 9.9.4 / 9.8.5: destination exists and Overwrite is F.
        Err(StorageError::AlreadyExists) => (
            StatusCode::PRECONDITION_FAILED,
            "destination exists and Overwrite is F",
        )
            .into_response(),
        Err(err) => storage_error_response(err),
    }
}

/// Extract, percent-decode, strip any DAV mount prefix from, and parse
/// the `Destination` header into a storage-relative `DavPath`. Shared
/// with the chunked-upload finalize MOVE (`handlers::uploads`), which
/// takes the same header to name where the assembled file should land.
///
/// Returns a plain `(StatusCode, message)` rather than a full `Response`
/// on error (clippy's `result_large_err`: `Response` itself is large
/// enough that returning it by value in the error case is wasteful) --
/// callers turn that into a response with `.into_response()`.
pub(crate) fn extract_destination_path(
    headers: &HeaderMap,
) -> Result<DavPath, (StatusCode, &'static str)> {
    let destination_header = headers
        .get("Destination")
        .and_then(|v| v.to_str().ok())
        .ok_or((StatusCode::BAD_REQUEST, "missing Destination header"))?;

    let dest_path_raw = extract_path(destination_header);
    let decoded = percent_decode_str(&dest_path_raw)
        .decode_utf8()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Destination is not valid UTF-8"))?;
    let (_prefix, dav_relative) = crate::split_mount_prefix(&decoded);
    DavPath::parse(dav_relative).map_err(|_| (StatusCode::BAD_REQUEST, "invalid Destination path"))
}

/// Pull the path component out of a Destination header value. RFC 4918
/// says this is an absolute URI, but plenty of clients in the wild send a
/// bare absolute path, so both are accepted.
fn extract_path(value: &str) -> String {
    if let Some(idx) = value.find("://") {
        let after_scheme = &value[idx + 3..];
        match after_scheme.find('/') {
            Some(slash) => after_scheme[slash..].to_string(),
            None => "/".to_string(),
        }
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_from_absolute_uri() {
        assert_eq!(
            extract_path("http://example.com/webdav/a b.txt"),
            "/webdav/a b.txt"
        );
        assert_eq!(extract_path("https://example.com:8080/foo"), "/foo");
    }

    #[test]
    fn extract_path_from_bare_path() {
        assert_eq!(extract_path("/foo/bar.txt"), "/foo/bar.txt");
    }
}
