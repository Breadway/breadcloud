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

    let destination_header = match headers.get("Destination").and_then(|v| v.to_str().ok()) {
        Some(v) => v,
        None => return (StatusCode::BAD_REQUEST, "missing Destination header").into_response(),
    };

    let dest_path_raw = extract_path(destination_header);
    let decoded = match percent_decode_str(&dest_path_raw).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "Destination is not valid UTF-8").into_response()
        }
    };
    let destination = match DavPath::parse(&decoded) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid Destination path").into_response(),
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
        Ok(WriteOutcome::Created) => StatusCode::CREATED.into_response(),
        Ok(WriteOutcome::Overwritten) => StatusCode::NO_CONTENT.into_response(),
        // RFC 4918 9.9.4 / 9.8.5: destination exists and Overwrite is F.
        Err(StorageError::AlreadyExists) => (
            StatusCode::PRECONDITION_FAILED,
            "destination exists and Overwrite is F",
        )
            .into_response(),
        Err(err) => storage_error_response(err),
    }
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
