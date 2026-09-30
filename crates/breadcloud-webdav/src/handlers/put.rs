use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, WriteOutcome};
use futures_util::StreamExt;

use crate::error::storage_error_response;
use crate::preconditions;
use crate::state::AppState;

/// Maximum plain WebDAV PUT size. Request data is streamed to disk, so
/// this limit bounds storage use rather than process memory.
const MAX_BODY_BYTES: usize = 20 * 1024 * 1024 * 1024;

pub async fn handle(state: &AppState, path: &DavPath, req: Request) -> Response {
    handle_with_limit(state, path, req, MAX_BODY_BYTES as u64).await
}

async fn handle_with_limit(
    state: &AppState,
    path: &DavPath,
    req: Request,
    max_bytes: u64,
) -> Response {
    if path.is_root() {
        return (StatusCode::CONFLICT, "cannot PUT to the storage root").into_response();
    }

    let headers = req.headers().clone();

    if let Some(conflict) = preconditions::check(state, path, &headers).await {
        return conflict;
    }

    let mut prepared = match state.storage.prepare_write(path).await {
        Ok(prepared) => prepared,
        Err(err) => return storage_error_response(err),
    };
    let mut body = req.into_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return (StatusCode::BAD_REQUEST, "could not read request body").into_response()
            }
        };
        if prepared.total().saturating_add(chunk.len() as u64) > max_bytes {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
        }
        if let Err(err) = prepared.write_chunk(&chunk).await {
            return storage_error_response(err);
        }
    }

    // CLOUD-11: reject a write that would exceed this account's quota
    // before it touches disk at all. `quota_gate` also closes the TOCTOU
    // window a bare `check_quota` call would leave open: it holds this
    // account's quota mutex from here until `quota_guard` drops (end of
    // this function), so a concurrent PUT/chunked-finalize against the
    // same account cannot observe "room available" for bytes this write
    // already claimed. Also carries the before/after usage this call
    // computed (CLOUD-13's `bread.cloud.quota.warning`, emitted below).
    let quota_guard = match state.quota_limit_bytes {
        Some(limit) => match state
            .storage
            .quota_gate(limit, path, prepared.total())
            .await
        {
            Ok(guard) => Some(guard),
            Err(err) => return storage_error_response(err),
        },
        None => None,
    };

    // RFC 4918 9.7.1: PUT creates the resource if it does not exist and
    // overwrites it if it does; the parent collection must already
    // exist, which `write_bytes` enforces (409 Conflict otherwise).
    // Snapshot only after the body and quota have been accepted, and
    // immediately before the atomic replacement is committed.
    if let Err(resp) = crate::versioning::snapshot_before_overwrite(state, path).await {
        return resp;
    }
    let (outcome, total, etag) = match prepared.commit(&state.storage).await {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

    // CLOUD-13: this write landed, so it counts as a sync. Best-effort,
    // see `breadcloud_events` module docs.
    breadcloud_events::file_synced(
        state.owner_label(),
        &path.to_request_path(),
        breadcloud_events::FileOp::Put,
        Some(total),
    );
    if let Some(guard) = &quota_guard {
        if let Some(percent) = breadcloud_events::crossed_threshold(
            guard.check.used_before,
            guard.check.used_after,
            guard.check.limit_bytes,
        ) {
            breadcloud_events::quota_warning(
                state.owner_label(),
                guard.check.used_after,
                guard.check.limit_bytes,
                percent,
            );
        }
    }

    let mtime_accepted = match crate::mtime::parse_oc_mtime(&headers) {
        Some(mtime) => crate::mtime::apply(state.storage.resolve(path), mtime),
        None => false,
    };

    let status = match outcome {
        WriteOutcome::Created => StatusCode::CREATED,
        WriteOutcome::Overwritten => StatusCode::NO_CONTENT,
    };
    let mut builder = Response::builder()
        .status(status)
        .header("ETag", etag)
        .header("OC-FileId", crate::fileid::for_path(path));
    if mtime_accepted {
        // Confirms to the client that its X-OC-Mtime was applied; without
        // this real Nextcloud clients assume the server ignored it (see
        // `mtime::apply`).
        builder = builder.header("X-OC-MTime", "accepted");
    }
    builder.body(Body::empty()).unwrap()
}

#[cfg(test)]
mod streaming_tests {
    use super::*;

    fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::TempDir::new().unwrap();
        let live = dir.path().join("live");
        let uploads = dir.path().join("uploads");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&uploads).unwrap();
        let state = AppState::new(
            breadcloud_storage::Storage::new(live),
            breadcloud_storage::Storage::new(uploads),
            breadcloud_storage::VersionStore::new(dir.path().join("versions")),
            breadcloud_storage::TrashStore::new(dir.path().join("trash")),
        );
        (dir, state)
    }
    fn assert_no_staging(dir: &std::path::Path) {
        for name in [".live-assembly-staging", ".uploads-assembly-staging"] {
            let stage = dir.join(name);
            if stage.exists() {
                assert_eq!(std::fs::read_dir(stage).unwrap().count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn rejected_and_interrupted_streams_preserve_existing_content() {
        let (dir, state) = state();
        let path = DavPath::parse("/file.txt").unwrap();
        let storage = &state.storage;
        storage.write_bytes(&path, b"old bytes").await.unwrap();
        for interrupted in [false, true] {
            let chunks = if interrupted {
                vec![
                    Ok(bytes::Bytes::from_static(b"new")),
                    Err(std::io::Error::other("connection interrupted")),
                ]
            } else {
                vec![
                    Ok(bytes::Bytes::from_static(b"new")),
                    Ok(bytes::Bytes::from_static(b" data")),
                ]
            };
            let body = Body::from_stream(futures_util::stream::iter(chunks));
            let req = Request::builder().method("PUT").body(body).unwrap();
            let response = handle_with_limit(&state, &path, req, 4).await;
            assert_eq!(
                response.status(),
                if interrupted {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
            );
            assert_eq!(storage.read_bytes(&path).await.unwrap(), b"old bytes");
            assert!(state.versions.list(&path).await.unwrap().is_empty());
            assert_no_staging(dir.path());
        }
    }

    #[tokio::test]
    async fn streamed_overwrite_retains_old_content_in_version_history() {
        use tokio::io::AsyncReadExt;
        let (_dir, state) = state();
        let path = DavPath::parse("/file.txt").unwrap();
        state
            .storage
            .write_bytes(&path, b"old bytes")
            .await
            .unwrap();
        let req = Request::builder()
            .method("PUT")
            .body(Body::from("new bytes"))
            .unwrap();
        assert_eq!(
            handle_with_limit(&state, &path, req, 32).await.status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(state.storage.read_bytes(&path).await.unwrap(), b"new bytes");
        let versions = state.versions.list(&path).await.unwrap();
        assert_eq!(versions.len(), 1);
        let (mut file, _) = state.versions.read(&path, &versions[0].id).await.unwrap();
        let mut old = Vec::new();
        file.read_to_end(&mut old).await.unwrap();
        assert_eq!(old, b"old bytes");
    }

    #[tokio::test]
    async fn version_copy_failure_preserves_live_content_and_cleans_staging() {
        let (dir, state) = state();
        let path = DavPath::parse("/file.txt").unwrap();
        state
            .storage
            .write_bytes(&path, b"old bytes")
            .await
            .unwrap();
        std::fs::write(dir.path().join("versions"), b"blocked directory").unwrap();
        let req = Request::builder()
            .method("PUT")
            .body(Body::from("new bytes"))
            .unwrap();
        assert_eq!(
            handle_with_limit(&state, &path, req, 32).await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(state.storage.read_bytes(&path).await.unwrap(), b"old bytes");
        assert_no_staging(dir.path());
    }
    #[tokio::test]
    async fn cancelling_an_inflight_upload_cleans_staging_and_preserves_live_bytes() {
        let (dir, state) = state();
        let path = DavPath::parse("/file.txt").unwrap();
        state
            .storage
            .write_bytes(&path, b"old bytes")
            .await
            .unwrap();
        let cloned = state.clone();
        let target = path.clone();
        let task = tokio::spawn(async move {
            let stream = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"partial"))
            })
            .chain(futures_util::stream::pending());
            let request = Request::builder()
                .method("PUT")
                .body(Body::from_stream(stream))
                .unwrap();
            handle_with_limit(&cloned, &target, request, 32).await
        });
        let staging = dir.path().join(".live-assembly-staging");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if std::fs::read_dir(&staging).is_ok_and(|entries| entries.count() > 0) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_no_staging(dir.path());
        assert_eq!(state.storage.read_bytes(&path).await.unwrap(), b"old bytes");
        assert!(state.versions.list(&path).await.unwrap().is_empty());
    }
}
