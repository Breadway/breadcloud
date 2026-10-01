//! RFC 7232 conditional-request preconditions (`If-Match` /
//! `If-None-Match`), the mechanism mirall and other WebDAV sync clients
//! use to avoid silently clobbering a change they have not seen yet.
//!
//! Shared between the plain PUT path (`handlers::put`) and the
//! chunked-upload finalize path (`handlers::uploads::finalize`): both
//! write to a destination in the main tree and both must honor the same
//! conditional headers before that write happens, or the guarantee only
//! holds for files small enough to skip chunking.

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use breadcloud_events::ConflictReason;
use breadcloud_storage::{DavPath, Storage, StorageError};

use crate::error::storage_error_response;
use crate::state::AppState;

/// Checks `If-Match`/`If-None-Match` against `path`'s current etag in
/// `state.storage`.
///
/// - `If-Match` guards an update: the client read the resource at some
///   etag and only wants its write to apply if nothing has changed it
///   since. A mismatch (or the resource having disappeared) means
///   someone else changed it first -- 412, so the client surfaces a sync
///   conflict instead of overwriting.
/// - `If-None-Match: *` guards a create: the client believes the resource
///   does not exist yet and wants its write to apply only if that is
///   still true, avoiding a race with another client creating the same
///   file.
///
/// Returns `Some(response)` to short-circuit the request with 412
/// Precondition Failed, `None` to proceed with the write.
pub async fn check(state: &AppState, path: &DavPath, headers: &HeaderMap) -> Option<Response> {
    check_storage(&state.storage, state.owner_label(), path, headers).await
}

/// Checks the same conditional headers against an arbitrary storage root.
/// CalDAV and CardDAV use this for their independent event/contact stores,
/// while plain WebDAV uses [`check`] above.
pub async fn check_storage(
    storage: &Storage,
    owner: Option<&str>,
    path: &DavPath,
    headers: &HeaderMap,
) -> Option<Response> {
    let current_etag = match storage.etag(path).await {
        Ok(etag) => Some(etag),
        Err(StorageError::NotFound) => None,
        Err(err) => return Some(storage_error_response(err)),
    };

    if let Some(value) = headers.get("If-None-Match").and_then(|v| v.to_str().ok()) {
        if value.trim() == "*" {
            if current_etag.is_some() {
                return Some(precondition_failed(
                    owner,
                    path,
                    ConflictReason::IfNoneMatch,
                    "If-None-Match: * but the resource already exists",
                ));
            }
        } else if let Some(etag) = &current_etag {
            if etag_list_contains(value, etag) {
                return Some(precondition_failed(
                    owner,
                    path,
                    ConflictReason::IfNoneMatch,
                    "If-None-Match matched the resource's current etag",
                ));
            }
        }
    }

    if let Some(value) = headers.get("If-Match").and_then(|v| v.to_str().ok()) {
        match &current_etag {
            None => {
                return Some(precondition_failed(
                    owner,
                    path,
                    ConflictReason::IfMatch,
                    "If-Match sent but the resource does not exist",
                ))
            }
            Some(etag) => {
                if value.trim() != "*" && !etag_list_contains(value, etag) {
                    return Some(precondition_failed(
                        owner,
                        path,
                        ConflictReason::IfMatch,
                        "If-Match did not match: the resource changed since it was last read",
                    ));
                }
            }
        }
    }

    None
}

/// `If-Match`/`If-None-Match` values are a comma-separated list of quoted
/// etags. Matching by exact string (after trimming whitespace around each
/// list entry) is sufficient for the etags this server issues, since it
/// never emits the `W/` weak-etag prefix.
fn etag_list_contains(header_value: &str, etag: &str) -> bool {
    header_value.split(',').map(str::trim).any(|v| v == etag)
}

/// Builds the 412 response and, alongside it, publishes
/// `bread.cloud.sync.conflict` (CLOUD-13): this precondition failure is
/// exactly the case a real sync client turns into a "conflicted copy"
/// instead of silently overwriting or losing a change, so it is the one
/// place both PUT and chunked-upload finalize share to detect a conflict
/// (see this module's doc comment).
fn precondition_failed(
    owner: Option<&str>,
    path: &DavPath,
    reason: ConflictReason,
    message: &str,
) -> Response {
    breadcloud_events::sync_conflict(owner, &path.to_request_path(), reason);
    (
        axum::http::StatusCode::PRECONDITION_FAILED,
        message.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_list_contains_matches_one_of_several() {
        let header = "\"abc\", \"def\"";
        assert!(etag_list_contains(header, "\"def\""));
        assert!(!etag_list_contains(header, "\"xyz\""));
    }
}
