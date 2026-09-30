use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, EntryKind, StorageError, TrashEntry, WriteOutcome};
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

    let prepared = match state
        .storage
        .prepare_relocation(source, &destination, overwrite, copy)
        .await
    {
        Ok(prepared) => prepared,
        Err(StorageError::AlreadyExists) => {
            return (
                StatusCode::PRECONDITION_FAILED,
                "destination exists and Overwrite is F",
            )
                .into_response()
        }
        Err(error) => return storage_error_response(error),
    };
    let mut retained = None;
    if prepared.existed() {
        let destination_meta = match state.storage.metadata(&destination).await {
            Ok(meta) => meta,
            Err(error) => {
                prepared.discard().await;
                return storage_error_response(error);
            }
        };
        if copy && !prepared.source_is_dir() && !destination_meta.is_dir() {
            // File COPY keeps destination history just like PUT. MOVE and
            // collection replacements retain the displaced resource in trash.
            if let Err(response) =
                crate::versioning::snapshot_before_overwrite(state, &destination).await
            {
                prepared.discard().await;
                return response;
            }
        } else {
            match retain_destination(state, &destination, destination_meta.is_dir()).await {
                Ok(entry) => retained = Some(entry),
                Err(error) => {
                    prepared.discard().await;
                    return storage_error_response(error);
                }
            }
        }
    }
    let result = state.storage.commit_relocation(prepared).await;
    if let Some(entry) = retained {
        if result.is_err() {
            if let Err(error) = restore_retained(state, &entry, &destination).await {
                return storage_error_response(error);
            }
        } else if let Err(error) = state.trash.enforce_caps().await {
            tracing::warn!("trash caps failed after replacement: {error}");
        }
    }

    match result {
        Ok(outcome) => {
            // Version history follows MOVE's source. Displaced destination
            // history was retained with its payload in trash before commit.
            // COPY keeps existing destination versions and snapshots the
            // prior file content, just like PUT.
            if !copy {
                if let Err(err) = state.versions.move_all(source, &destination).await {
                    tracing::warn!(
                        "failed to carry version history from {source} to {destination}: {err}"
                    );
                }
                // CLOUD-13: a MOVE is a sync (a rename); a COPY produces
                // an independent new resource and is deliberately not
                // reported (see `breadcloud_events::FileOp`). `bytes` is
                // omitted: unlike PUT/chunked-upload finalize, nothing on
                // this path already knows the resulting size, and `source`
                // may itself have been a directory. Best-effort, see
                // `breadcloud_events` module docs.
                breadcloud_events::file_synced(
                    state.owner_label(),
                    &destination.to_request_path(),
                    breadcloud_events::FileOp::Move,
                    None,
                );
            }
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

async fn retain_destination(
    state: &AppState,
    path: &DavPath,
    is_dir: bool,
) -> Result<TrashEntry, StorageError> {
    let size = if is_dir {
        state
            .storage
            .list_recursive(path)
            .await?
            .iter()
            .filter(|entry| !entry.metadata.is_dir())
            .map(|entry| entry.metadata.len)
            .sum()
    } else {
        state.storage.metadata(path).await?.len
    };
    let kind = if is_dir {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    let entry = state
        .trash
        .trash(&state.storage.resolve(path), path, kind, size)
        .await?;
    if let Err(error) = state
        .versions
        .move_out(path, &state.trash.versions_dir(&entry.id))
        .await
    {
        // Failed move_out normally leaves history at its original path.
        // Restore payload alone, preserving stranded data on RecoveryFailed.
        if matches!(error, StorageError::RecoveryFailed { .. }) {
            return Err(error);
        }
        state
            .trash
            .restore_payload(&entry.id, &state.storage.resolve(path))
            .await?;
        state.trash.finish_restore(&entry.id).await?;
        return Err(error);
    }
    Ok(entry)
}

async fn restore_retained(
    state: &AppState,
    entry: &TrashEntry,
    destination: &DavPath,
) -> Result<(), StorageError> {
    state
        .trash
        .restore_payload(&entry.id, &state.storage.resolve(destination))
        .await?;
    state
        .versions
        .move_in(destination, &state.trash.versions_dir(&entry.id))
        .await?;
    state.trash.finish_restore(&entry.id).await
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
