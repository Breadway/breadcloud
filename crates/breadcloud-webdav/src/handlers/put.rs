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
        if prepared.total().saturating_add(chunk.len() as u64) > MAX_BODY_BYTES as u64 {
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
