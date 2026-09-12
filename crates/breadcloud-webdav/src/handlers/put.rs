use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, WriteOutcome};

use crate::error::storage_error_response;
use crate::preconditions;
use crate::state::AppState;

/// Safety cap on a buffered PUT body. Individual files in the target tree
/// can be multiple gigabytes; buffering the whole request in memory is
/// fine at this size for the plain (non-chunked) PUT path -- real clients
/// switch to chunked upload (see `handlers::uploads`) well before their
/// files get anywhere near this cap.
const MAX_BODY_BYTES: usize = 20 * 1024 * 1024 * 1024;

pub async fn handle(state: &AppState, path: &DavPath, req: Request) -> Response {
    if path.is_root() {
        return (StatusCode::CONFLICT, "cannot PUT to the storage root").into_response();
    }

    let headers = req.headers().clone();

    if let Some(conflict) = preconditions::check(state, path, &headers).await {
        return conflict;
    }

    // Snapshot whatever is currently at `path` into version history
    // (CLOUD-4) before the write below can replace it. A no-op if `path`
    // does not exist yet -- first creation is not a "previous version".
    if let Err(resp) = crate::versioning::snapshot_before_overwrite(state, path).await {
        return resp;
    }

    let bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read request body").into_response(),
    };

    // RFC 4918 9.7.1: PUT creates the resource if it does not exist and
    // overwrites it if it does; the parent collection must already
    // exist, which `write_bytes` enforces (409 Conflict otherwise).
    let (outcome, etag) = match state.storage.write_bytes(path, &bytes).await {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

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
