//! A Nextcloud-trashbin-shaped DAV surface for listing, restoring, and
//! permanently purging soft-deleted resources (CLOUD-5), mounted at
//! `/remote.php/dav/trashbin/<user>/...`.
//!
//! Real Nextcloud addresses a trashed item by its persistent numeric
//! fileid plus a deletion timestamp
//! (`dav/trashbin/<user>/trash/<fileid>.d<timestamp>`). This server has no
//! such fileid (see `crate::fileid`'s doc comment, and the identical scope
//! note on `handlers::versions`), so entries here are addressed by
//! `TrashStore`'s own opaque id instead: `PROPFIND
//! /remote.php/dav/trashbin/<user>/` lists every trashed item as a child
//! href `/remote.php/dav/trashbin/<user>/<id>`, `MOVE` on one of those
//! hrefs restores it (`Destination` optional -- omit it to restore to the
//! original path, or give one when that path is occupied), and `DELETE`
//! on one permanently purges it early.
//!
//! Scoped down the same way `handlers::versions` is: there is no way to
//! browse *inside* a trashed directory's own subtree here (it was deleted
//! as one unit and is restored or purged as that same unit, matching how
//! DELETE removed it recursively in the first place), and no GET to fetch
//! trashed content directly -- the issue asks for list/restore/purge, not
//! a download endpoint, so this does not add one. That is enough to
//! browse what is in trash and get it back; it is not a drop-in
//! implementation of Nextcloud's actual trashbin protocol (see the
//! docs/DESIGN.md for this scope decision, the same tradeoff CLOUD-4
//! made for its own versions surface).
//!
//! The `<user>` path segment mirrors `handlers::uploads`/`handlers::versions`'s
//! layout, and is validated the same way theirs is (CLOUD-11: see
//! `handlers::versions`'s own doc comment for where and how -- the
//! combined server's `url_scoped_user` check, not this handler itself).

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, EntryKind, StorageError};

use crate::error::{multi_status, storage_error_response};
use crate::handlers::copy_move::extract_destination_path;
use crate::state::AppState;

pub async fn dispatch(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    remainder: &str,
) -> Response {
    let path = match DavPath::parse(remainder) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid trashbin path").into_response(),
    };
    let segments = path.segments();
    if segments.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            "expected /remote.php/dav/trashbin/<user>/...",
        )
            .into_response();
    }
    // First segment is the user (see the module doc comment for why this
    // is not validated further); the rest, if present, names a specific
    // trash entry by id.
    let rest = &segments[1..];

    match (method.as_str(), rest.len()) {
        ("PROPFIND", 0) => propfind_trashbin(state, &trashbin_prefix(&segments[0])).await,
        ("MOVE", 1) => restore(state, &rest[0], headers).await,
        ("DELETE", 1) => purge(state, &rest[0]).await,
        ("PROPFIND", _) | ("MOVE", _) | ("DELETE", _) => (
            StatusCode::NOT_FOUND,
            "expected /remote.php/dav/trashbin/<user>/ (PROPFIND) or /remote.php/dav/trashbin/<user>/<id> (MOVE/DELETE)",
        )
            .into_response(),
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "only PROPFIND, MOVE and DELETE are supported on the trashbin surface",
        )
            .into_response(),
    }
}

fn trashbin_prefix(user: &str) -> String {
    format!("/remote.php/dav/trashbin/{user}")
}

/// Lists every trashed entry as a plain `DAV:`-only multistatus, plus two
/// `nc:`-namespaced properties naming where each item came from and when
/// it was deleted -- the two facts the issue asks PROPFIND to surface.
/// Named after real Nextcloud's own `nc:trashbin-original-location` /
/// `nc:trashbin-deletion-time` properties (mirroring the existing
/// client-facing convention, per the module doc comment's scope note),
/// not a claim of full protocol compatibility.
async fn propfind_trashbin(state: &AppState, prefix: &str) -> Response {
    let entries = match state.trash.list().await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };

    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    body.push_str("<d:multistatus xmlns:d=\"DAV:\" xmlns:nc=\"http://owncloud.org/ns\">\n");
    for e in &entries {
        let href = format!("{prefix}/{}", e.id);
        body.push_str("  <d:response>\n    <d:href>");
        body.push_str(&xml_escape(&href));
        body.push_str("</d:href>\n");
        body.push_str("    <d:propstat>\n      <d:prop>\n");
        if e.kind == EntryKind::Directory {
            body.push_str("        <d:resourcetype><d:collection/></d:resourcetype>\n");
        } else {
            body.push_str("        <d:resourcetype/>\n");
        }
        body.push_str(&format!(
            "        <d:getcontentlength>{}</d:getcontentlength>\n",
            e.size
        ));
        body.push_str("        <d:getlastmodified>");
        body.push_str(&xml_escape(&httpdate::fmt_http_date(e.deleted_at)));
        body.push_str("</d:getlastmodified>\n");
        body.push_str("        <nc:trashbin-original-location>");
        body.push_str(&xml_escape(&e.original_path.to_request_path()));
        body.push_str("</nc:trashbin-original-location>\n");
        body.push_str("        <nc:trashbin-deletion-time>");
        body.push_str(&xml_escape(&unix_seconds(e.deleted_at).to_string()));
        body.push_str("</nc:trashbin-deletion-time>\n");
        body.push_str(
            "      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n",
        );
        body.push_str("  </d:response>\n");
    }
    body.push_str("</d:multistatus>\n");

    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// Restores trash entry `id` back onto the live tree. `Destination`, if
/// given, names where to restore it (for when the original path is now
/// occupied); otherwise it restores to the original path recorded at
/// deletion time.
///
/// Unlike the main MOVE handler (`handlers::copy_move`), `Overwrite`
/// defaults to *false* here rather than true: silently replacing whatever
/// now occupies the restore path with old, trashed content is a worse
/// default than making the caller opt in with an explicit `Overwrite: T`.
/// When it is given and the destination is occupied, whatever is there is
/// eventually discarded (not itself trashed or versioned) -- the same
/// behavior `handlers::copy_move`'s own MOVE-with-Overwrite already has
/// for the identical "a live resource gets replaced" situation, not a new
/// special case. Its own version history, if any, is safely discarded too,
/// as part of the same `crate::move_path`-backed replace `VersionStore::move_in`
/// uses for the version-history move below (see that function's doc
/// comment), so nothing is orphaned there either.
///
/// The version history and the payload are moved in that order,
/// deliberately, and that order is itself the fix for a real data-loss bug
/// this handler used to have: an earlier version moved the payload first,
/// and if the version-history move then failed, the trash entry was
/// (correctly) left in place rather than purged -- but the payload had
/// already landed on the live tree by that point, so the entry looked
/// "occupied, retry with Overwrite: T" to a client, and a retry along that
/// exact path deleted the just-restored file and then failed to find a
/// payload to put back (it had already left the entry in the first
/// attempt), destroying the file for good.
///
/// Moving the version history first instead means the payload -- the only
/// thing a client can observe land on the live tree -- is always the last
/// move this handler makes. Every early return above that move leaves the
/// live tree completely untouched and the trash entry fully intact, so a
/// retry always starts clean rather than racing a half-restored file. If
/// the payload move itself then fails after the version history already
/// moved, that move is rolled back (moved back into the trash entry)
/// rather than left stranded on the live side with nothing pointing a
/// future retry back to it.
///
/// Neither move below needs this handler to pre-clean `dest_path` by hand
/// (an earlier version of this code explicitly deleted whatever was at
/// `dest_path` before attempting the restore when `Overwrite: T` was
/// given, and cleaned up partial content after each failed move itself):
/// both `VersionStore::move_in` and `TrashStore::restore_payload` go
/// through `crate::move_path` (see `breadcloud-storage/src/lib.rs`'s doc
/// comment on it for the full safety argument), the shared safe
/// "replace a destination with a source" primitive that never touches a
/// destination's existing content until the replacement is fully staged
/// and verified, and restores everything automatically if the move fails
/// partway. So an ordinary failure at either move below is guaranteed to
/// leave `dest_path` and the trash entry in a well-defined, safely
/// retryable state without this handler doing any cleanup of its own; the
/// only thing still handled here by hand is a matter of restore semantics
/// rather than move safety: a failed payload move must roll the
/// already-succeeded version-history move back.
///
/// The one exception to "safely retryable" is `StorageError::RecoveryFailed`:
/// `move_path`'s own unwind can itself fail (the same class of fault that
/// broke the primary move can just as easily hit the compensating rename a
/// moment later), and when it does, content is left stranded at a
/// stage/backup path rather than safely back at its original location.
/// Both places below that call into `move_path` (directly or via the
/// rollback) check for this distinctly and log it at `error!` with the
/// exact stranded path(s) `RecoveryFailed` names, rather than treating it
/// as an ordinary safe-to-retry failure.
async fn restore(state: &AppState, id: &str, headers: &HeaderMap) -> Response {
    let entry = match state.trash.get(id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };

    let dest_path = if headers.get("Destination").is_some() {
        match extract_destination_path(headers) {
            Ok(p) => p,
            Err((status, msg)) => return (status, msg).into_response(),
        }
    } else {
        entry.original_path.clone()
    };

    if dest_path.is_root() {
        return (
            StatusCode::FORBIDDEN,
            "cannot restore a trash entry over the storage root",
        )
            .into_response();
    }

    let overwrite = headers
        .get("Overwrite")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("T"))
        .unwrap_or(false);

    let existed = state.storage.exists(&dest_path).await;
    if existed && !overwrite {
        return (
            StatusCode::PRECONDITION_FAILED,
            "restore destination exists; retry with Overwrite: T or a different Destination",
        )
            .into_response();
    }

    // A restore adds the trashed payload back to live storage. Keep the
    // quota reservation until both payload and version history have moved.
    let _quota_guard = match state.quota_limit_bytes {
        Some(limit) => match state
            .storage
            .quota_gate(limit, &dest_path, entry.size)
            .await
        {
            Ok(guard) => Some(guard),
            Err(err) => return storage_error_response(err),
        },
        None => None,
    };

    let dest_fs_path = state.storage.resolve(&dest_path);
    let versions_dir = state.trash.versions_dir(id);

    // Version history moves first. An ordinary failure here leaves the
    // trash entry fully intact (payload untouched, still in trash) and the
    // live tree untouched (the payload move has not run yet, and
    // `move_in` itself never partially lands content at `dest_path` on
    // failure), so this is always safe to retry or abandon -- see the doc
    // comment above. `StorageError::RecoveryFailed` is the one exception:
    // it means `move_in`'s own error-recovery also failed, so content is
    // stranded at a stage/backup path rather than safely at either the
    // trash entry or `dest_path`, and must not be assumed safe to retry
    // without a look first.
    if let Err(err) = state.versions.move_in(&dest_path, &versions_dir).await {
        if matches!(err, StorageError::RecoveryFailed { .. }) {
            tracing::error!(
                "failed to move version history for {id} onto {dest_path}, AND that \
                 failure's own recovery also failed: {err}; this needs manual operator \
                 attention before any further retry of this restore"
            );
        } else {
            tracing::error!(
                "failed to move version history for {id} onto {dest_path}: {err}; \
                 trash entry {id} and the live tree are both unchanged, safe to retry"
            );
        }
        return storage_error_response(err);
    }

    // Payload moves last. If this fails, the version history already
    // moved above must be rolled back into the trash entry: otherwise it
    // would be stranded on the live-tree side with no corresponding live
    // file, and nothing left to roll a later retry back to. The rollback
    // is itself just another `crate::move_path`-backed call, so an
    // ordinary failure there is equally safe: the live-mirror history it
    // is trying to move is never touched until the trash-side copy of it
    // is confirmed staged, so it cannot be lost even if this rollback
    // itself fails in the ordinary way. It is possible for the rollback's
    // own error-recovery to fail too, though (`StorageError::RecoveryFailed`):
    // that is a distinct, worse outcome where the history is not safely at
    // either the live-mirror path or back in the trash entry, and is
    // logged as such below rather than folded into the same message.
    if let Err(err) = state.trash.restore_payload(id, &dest_fs_path).await {
        tracing::error!(
            "failed to move payload for {id} onto {dest_path}: {err}; rolling back the \
             version history already moved above so trash entry {id} stays fully \
             restorable"
        );
        if let Err(rollback_err) = state.versions.move_out(&dest_path, &versions_dir).await {
            if matches!(rollback_err, StorageError::RecoveryFailed { .. }) {
                tracing::error!(
                    "failed to roll back version history for {id} after a failed payload \
                     restore, AND that rollback's own recovery also failed: {rollback_err}; \
                     the history is NOT safely at the live-mirror path or back in trash \
                     entry {id}, this needs manual operator attention before any further \
                     retry of this restore"
                );
            } else {
                tracing::error!(
                    "failed to roll back version history for {id} after a failed payload \
                     restore: {rollback_err}; the version history itself is safe (a failed \
                     move never destroys its source), left at the live-mirror path rather \
                     than back in trash entry {id}; a further retry of this restore will \
                     pick up correctly from there, but this is still worth operator \
                     attention"
                );
            }
        }
        return storage_error_response(err);
    }
    state.storage.invalidate_etag_cache(&dest_path);

    if let Err(err) = state.trash.finish_restore(id).await {
        tracing::warn!("failed to clean up trash entry {id} after restore: {err}");
    }

    if let Err(err) = state.trash.enforce_caps().await {
        tracing::warn!("trash retention cap enforcement failed after restoring {id}: {err}");
    }

    let status = if existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    match state.storage.etag(&dest_path).await {
        Ok(etag) => Response::builder()
            .status(status)
            .header("ETag", etag)
            .body(Body::empty())
            .unwrap(),
        Err(_) => status.into_response(),
    }
}

/// Permanently purges trash entry `id` early, without waiting for the
/// retention caps to reclaim it.
async fn purge(state: &AppState, id: &str) -> Response {
    // Confirm the entry exists first, so an unknown id gets a 404 rather
    // than the same 204 a successful purge would (TrashStore::purge
    // itself treats an already-gone id as a no-op, matching this crate's
    // other idempotent-delete conventions).
    if let Err(err) = state.trash.get(id).await {
        return storage_error_response(err);
    }
    match state.trash.purge(id).await {
        Ok(()) => {
            if let Err(err) = state.trash.enforce_caps().await {
                tracing::warn!("trash retention cap enforcement failed after purging {id}: {err}");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => storage_error_response(err),
    }
}

fn unix_seconds(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
