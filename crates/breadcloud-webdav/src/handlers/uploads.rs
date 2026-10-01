//! Nextcloud's chunked upload protocol (NG chunking v2, what mirall's own
//! source and docs call "new chunking"), mounted at
//! `/remote.php/dav/uploads/<user>/<transfer-id>[/<chunk-name>]`.
//!
//! The flow a real client drives:
//!
//! 1. `MKCOL /remote.php/dav/uploads/<user>/<transfer-id>` -- create a
//!    staging collection for this upload. `<transfer-id>` is client-
//!    chosen (observed: a random hex string).
//! 2. `PUT .../  <transfer-id>/<n>` for each chunk, `<n>` a plain integer
//!    encoding chunk order (not necessarily contiguous from 0 -- only the
//!    relative order matters). Chunks may arrive out of order or be
//!    retried; this server does not care, it sorts by `<n>` at assembly
//!    time.
//! 3. `PROPFIND` (depth 1) on the transfer collection to see which
//!    chunks already landed, if a previous attempt was interrupted --
//!    this is the resumability mechanism: the client diffs this against
//!    its own chunk plan and only re-uploads what is missing.
//! 4. `MOVE` from a resource under the transfer collection (observed:
//!    `.../<transfer-id>/.file`, a virtual name that need not exist as an
//!    actual uploaded chunk) to the real destination path in the main
//!    tree, with a `Destination` header and an `OC-Total-Length` header
//!    giving the expected assembled size. This server assembles the
//!    chunks in numeric order, verifies the total against
//!    `OC-Total-Length`, and only then makes the result visible at
//!    `Destination` -- see `breadcloud_storage::Storage::assemble`.
//!
//! Chunks live in `AppState::uploads`, a `Storage` sandboxed to its own
//! root distinct from the main tree, so an in-progress upload is never
//! visible in a PROPFIND of the real tree and a crash mid-upload cannot
//! leave partial chunk files where a sync client might mistake them for
//! real content.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError, WriteOutcome};

use crate::error::{multi_status, storage_error_response};
use crate::handlers::copy_move::extract_destination_path;
use crate::preconditions;
use crate::state::AppState;
use crate::xml::{build_multistatus, PropfindRequest, ResourceEntry};

/// Safety cap on one buffered chunk. Real clients keep individual chunks
/// well under this (historically single-digit to low tens of MB); it
/// exists to bound memory, not to match any particular client's chunk
/// size.
const MAX_CHUNK_BYTES: usize = 512 * 1024 * 1024;

const UPLOADS_PREFIX: &str = "/remote.php/dav/uploads";

/// Entry point from `dispatch`. `remainder` is the request path with the
/// `/remote.php/dav/uploads` prefix already stripped, e.g.
/// `/dev/9c2f0.../1`.
pub async fn dispatch(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    remainder: &str,
    req: Request,
) -> Response {
    let upload_path = match DavPath::parse(remainder) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid chunked-upload path").into_response(),
    };

    match upload_path.segments().len() {
        2 => handle_transfer_collection(state, method, headers, &upload_path).await,
        3 => handle_chunk(state, method, headers, &upload_path, req).await,
        _ => (
            StatusCode::NOT_FOUND,
            "expected /remote.php/dav/uploads/<user>/<transfer-id>[/<chunk>]",
        )
            .into_response(),
    }
}

async fn handle_transfer_collection(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    transfer_dir: &DavPath,
) -> Response {
    match method.as_str() {
        "MKCOL" => {
            // The client never explicitly creates the per-user directory
            // first, so ensure it exists before creating the transfer
            // collection inside it.
            if let Some(user_dir) = transfer_dir.parent() {
                match state.uploads.mkcol(&user_dir).await {
                    Ok(()) | Err(StorageError::AlreadyExists) => {}
                    Err(err) => return storage_error_response(err),
                }
            }
            match state.uploads.mkcol(transfer_dir).await {
                Ok(()) => StatusCode::CREATED.into_response(),
                Err(StorageError::AlreadyExists) => (
                    StatusCode::METHOD_NOT_ALLOWED,
                    "transfer collection already exists",
                )
                    .into_response(),
                Err(err) => storage_error_response(err),
            }
        }
        "PROPFIND" => propfind_transfer(state, transfer_dir).await,
        "DELETE" => match state.uploads.delete(transfer_dir).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(err) => storage_error_response(err),
        },
        "MOVE" => finalize(state, transfer_dir, headers).await,
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method on a chunked-upload transfer collection",
        )
            .into_response(),
    }
}

async fn handle_chunk(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    chunk_path: &DavPath,
    req: Request,
) -> Response {
    match method.as_str() {
        "PUT" => {
            let bytes = match axum::body::to_bytes(req.into_body(), MAX_CHUNK_BYTES).await {
                Ok(b) => b,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "could not read chunk body").into_response()
                }
            };
            match state.uploads.write_bytes(chunk_path, &bytes).await {
                Ok((WriteOutcome::Created, _)) => StatusCode::CREATED.into_response(),
                Ok((WriteOutcome::Overwritten, _)) => StatusCode::NO_CONTENT.into_response(),
                Err(err) => storage_error_response(err),
            }
        }
        // The finalizing request targets a resource *under* the transfer
        // collection (observed: ".file"), not the collection itself, but
        // the source name is a convention only -- assembly always
        // operates on every chunk in the parent collection regardless of
        // what this particular path segment is called.
        "MOVE" => match chunk_path.parent() {
            Some(transfer_dir) => finalize(state, &transfer_dir, headers).await,
            None => (StatusCode::BAD_REQUEST, "invalid chunk path").into_response(),
        },
        "DELETE" => match state.uploads.delete(chunk_path).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(err) => storage_error_response(err),
        },
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method on a chunk resource",
        )
            .into_response(),
    }
}

/// PROPFIND on a transfer collection: reports the chunks already landed
/// (name + size) so an interrupted client can resume rather than restart
/// the whole upload. Etags here are the cheap mtime/len signature, not
/// content-hashed -- a chunk is an internal staging artifact the client
/// never asks for by etag, it only cares about which names/sizes exist.
async fn propfind_transfer(state: &AppState, transfer_dir: &DavPath) -> Response {
    let self_metadata = match state.uploads.metadata(transfer_dir).await {
        Ok(m) => m,
        Err(StorageError::NotFound) => {
            return (StatusCode::NOT_FOUND, "unknown transfer").into_response()
        }
        Err(err) => return storage_error_response(err),
    };

    let mut entries = vec![ResourceEntry {
        path: transfer_dir.clone(),
        etag: self_metadata.etag(),
        metadata: self_metadata,
        quota: None,
    }];

    match state.uploads.list_dir(transfer_dir).await {
        Ok(children) => {
            for child in children {
                entries.push(ResourceEntry {
                    etag: child.metadata.etag(),
                    path: child.path,
                    metadata: child.metadata,
                    quota: None,
                });
            }
        }
        Err(err) => return storage_error_response(err),
    }

    let xml = build_multistatus(&entries, &PropfindRequest::AllProp, UPLOADS_PREFIX);
    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

/// Assemble every chunk in `transfer_dir`, in ascending numeric order of
/// its name, into the path named by the `Destination` header, validating
/// the result against `OC-Total-Length` if the client sent one. On
/// success the transfer collection is removed; on failure it is left
/// alone so the client can inspect chunk state or retry.
async fn finalize(state: &AppState, transfer_dir: &DavPath, headers: &HeaderMap) -> Response {
    let dest = match extract_destination_path(headers) {
        Ok(p) => p,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    // Same If-Match/If-None-Match conflict detection plain PUT gets
    // (`preconditions::check`), checked against the destination in the
    // main tree before any chunk is assembled onto it. Without this a
    // chunked finalize (the path every file above the client's chunking
    // threshold takes) could silently overwrite a change the client
    // never saw, even though the equivalent plain PUT correctly 412s.
    if let Some(conflict) = preconditions::check(state, &dest, headers).await {
        return conflict;
    }

    let overwrite = headers
        .get("Overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|v| !v.eq_ignore_ascii_case("F"))
        .unwrap_or(true);

    let children = match state.uploads.list_dir(transfer_dir).await {
        Ok(c) => c,
        Err(StorageError::NotFound) => {
            return (StatusCode::NOT_FOUND, "unknown transfer").into_response()
        }
        Err(err) => return storage_error_response(err),
    };

    let mut numbered: Vec<(u64, PathBuf)> = children
        .iter()
        .filter(|entry| !entry.metadata.is_dir())
        .filter_map(|entry| {
            entry
                .path
                .name()
                .and_then(|name| name.parse::<u64>().ok())
                .map(|n| (n, state.uploads.resolve(&entry.path)))
        })
        .collect();
    numbered.sort_by_key(|(n, _)| *n);

    if numbered.is_empty() {
        return (StatusCode::CONFLICT, "no chunks uploaded for this transfer").into_response();
    }

    let chunk_paths: Vec<PathBuf> = numbered.into_iter().map(|(_, path)| path).collect();

    let expected_total = headers
        .get("OC-Total-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Validates the finalize against `dest` and assembles the chunks into
    // a temp file, without touching `dest` yet. Every failure mode that
    // depends on `dest` or the chunks (Overwrite:F conflict, OC-Total-Length
    // mismatch, a vanished chunk) surfaces here, before anything is
    // snapshotted or replaced -- see CLOUD-4 rework notes on why the
    // version snapshot must not happen any earlier than this.
    let prepared = match state
        .storage
        .prepare_assemble(&dest, &chunk_paths, overwrite, expected_total)
        .await
    {
        Ok(p) => p,
        Err(StorageError::AlreadyExists) => {
            return (
                StatusCode::PRECONDITION_FAILED,
                "destination exists and Overwrite is F",
            )
                .into_response()
        }
        Err(StorageError::LengthMismatch { expected, actual }) => {
            return (
                StatusCode::PRECONDITION_FAILED,
                format!("assembled {actual} bytes but OC-Total-Length declared {expected}"),
            )
                .into_response()
        }
        Err(err) => return storage_error_response(err),
    };

    // CLOUD-11: same quota gate plain PUT gets (`Storage::quota_gate`,
    // shared between both paths rather than two independent ad-hoc
    // locks), run here rather than earlier because `prepared.total()`
    // (the exact assembled size) is only known once `prepare_assemble`
    // has actually assembled the chunks into a staged temp file --
    // nothing has touched `dest` yet at this point, so discarding
    // `prepared` on a rejection leaves it (and everything else)
    // untouched, same guarantee plain PUT's pre-write check gets.
    //
    // `quota_guard` holds this account's quota mutex until it drops at
    // the end of this function, closing the same TOCTOU window plain PUT
    // closes: a concurrent PUT or chunked finalize against this account
    // cannot observe "room available" for bytes this finalize already
    // claimed. Also carries the before/after usage this call computed
    // (CLOUD-13's `bread.cloud.quota.warning`, emitted below).
    let quota_guard = match state.quota_limit_bytes {
        Some(limit) => match state
            .storage
            .quota_gate(limit, &dest, prepared.total())
            .await
        {
            Ok(guard) => Some(guard),
            Err(err) => {
                prepared.discard().await;
                return storage_error_response(err);
            }
        },
        None => None,
    };

    // Same version-history snapshot plain PUT gets (CLOUD-4). By this
    // point the finalize is known to actually replace `dest`'s content
    // once committed, so this can no longer fire for a finalize the
    // empty-chunks check, an Overwrite:F conflict, or a length mismatch
    // is about to reject. Most files in a real synced dataset take this
    // chunked path rather than plain PUT (see CLOUD-3), so skipping this
    // here would leave versioning working only for small files.
    let pending_snapshot = match crate::versioning::snapshot_before_overwrite(state, &dest).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            prepared.discard().await;
            return storage_error_response(err);
        }
    };

    match state.storage.commit_assemble(prepared).await {
        Ok((outcome, total, etag)) => {
            crate::versioning::finish_snapshot(state, &dest, pending_snapshot).await;
            let mtime_accepted = match crate::mtime::parse_oc_mtime(headers) {
                Some(mtime) => crate::mtime::apply(state.storage.resolve(&dest), mtime),
                None => false,
            };
            // Best-effort: the assembled file is already live at this
            // point, so a failure to clean up the staging directory is
            // not worth failing the request over.
            let _ = state.uploads.delete(transfer_dir).await;

            // CLOUD-13: this finalize committed the assembled file, so it
            // counts as a sync. Best-effort, see `breadcloud_events`
            // module docs.
            breadcloud_events::file_synced(
                state.owner_label(),
                &dest.to_request_path(),
                breadcloud_events::FileOp::ChunkedUpload,
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

            let status = match outcome {
                WriteOutcome::Created => StatusCode::CREATED,
                WriteOutcome::Overwritten => StatusCode::NO_CONTENT,
            };
            let mut builder = Response::builder()
                .status(status)
                .header("ETag", etag)
                .header("OC-FileId", crate::fileid::for_path(&dest));
            if mtime_accepted {
                builder = builder.header("X-OC-MTime", "accepted");
            }
            builder.body(Body::empty()).unwrap()
        }
        Err(err) => {
            crate::versioning::discard_snapshot(state, &dest, pending_snapshot).await;
            storage_error_response(err)
        }
    }
}
