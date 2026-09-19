//! bread ecosystem event integration for breadcloud (CLOUD-13).
//!
//! Mirrors breadclip's `AdapterSource::App` registry / command-as-event
//! convention: see the `bread` repo's `Documentation.md` ("Namespaces" and
//! "Integrating a bread* app" sections) and breadclip's own `EVENTS.md`,
//! which is the worked reference implementation this follows. breadcloud
//! registers as app id `cloud` (added to `bread-shared::apps::KNOWN_APPS`
//! in a companion change to the `bread` repo) and publishes under
//! `bread.cloud.*`.
//!
//! This integration is explicitly optional (see the workspace README):
//! breadcloud must behave identically whether or not `breadd` is
//! installed or running. Concretely:
//!
//! - Every publish here goes through [`BreadClient::emit`], which is
//!   already fail-silent by design: no socket, no reply, no error, it is
//!   simply dropped. See `bread-utils`'s own `bread_client` module docs.
//! - Every publish is additionally dispatched onto Tokio's blocking
//!   thread pool ([`tokio::task::spawn_blocking`]) rather than run
//!   inline on the caller's async task. `BreadClient::emit` opens a
//!   blocking `UnixStream` and writes to it under a 200ms write timeout;
//!   without this, a slow or wedged breadd could add up to that much
//!   latency to the WebDAV request that triggered the event. Dispatching
//!   it away means a request never waits on event delivery at all.
//! - Nothing here returns a `Result` a caller could feel obligated to
//!   check: there is nothing meaningful to do with a failure to notify
//!   an optional automation bus, and building error plumbing for it
//!   would misrepresent how load-bearing this path is (not at all).
//!
//! Event vocabulary (see the workspace README's own "bread ecosystem
//! integration" section for the full catalogue and each field's exact
//! meaning): `bread.cloud.file.synced` (a completed PUT, chunked-upload
//! finalize, DELETE, or MOVE), `bread.cloud.sync.conflict` (an
//! `If-Match`/`If-None-Match` precondition rejection, CLOUD-3), and
//! `bread.cloud.quota.warning` (usage crossing an 80/90/100 percent
//! threshold of a per-account quota, CLOUD-11).

use bread_utils::bread_client::BreadClient;
use serde_json::{json, Value};

/// This app's reserved segment in `bread.<id>.*` / `bread.command.<id>.*`.
pub const APP_ID: &str = "cloud";

/// The WebDAV operation a `bread.cloud.file.synced` event reports.
/// Deliberately covers only the four operations CLOUD-13 names: COPY is
/// excluded because it produces an independent new resource rather than
/// syncing existing content (the same distinction
/// `breadcloud-webdav::handlers::copy_move` already draws for whether
/// version history follows the operation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    /// A plain (non-chunked) `PUT`.
    Put,
    /// A chunked-upload (NG chunking v2) finalize `MOVE` that assembled
    /// and committed the staged chunks.
    ChunkedUpload,
    /// A `DELETE`, soft-deleted into trash (CLOUD-5).
    Delete,
    /// A `MOVE` (rename), not a `COPY`.
    Move,
}

impl FileOp {
    fn as_str(self) -> &'static str {
        match self {
            FileOp::Put => "put",
            FileOp::ChunkedUpload => "chunked_upload",
            FileOp::Delete => "delete",
            FileOp::Move => "move",
        }
    }
}

/// Which conditional-request precondition rejected a write
/// (`breadcloud-webdav::preconditions`, CLOUD-3's conflict detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// `If-Match` did not match the resource's current etag, or the
    /// resource no longer existed: someone else's write landed first.
    IfMatch,
    /// `If-None-Match: *` but the resource already existed: someone else
    /// created it first.
    IfNoneMatch,
}

impl ConflictReason {
    fn as_str(self) -> &'static str {
        match self {
            ConflictReason::IfMatch => "if-match",
            ConflictReason::IfNoneMatch => "if-none-match",
        }
    }
}

/// Publish `bread.cloud.file.synced` for a completed write or removal.
///
/// `owner` is the account username if known (the multi-user/CLOUD-11
/// combined server always knows it; a standalone/dev instance with no
/// accounts configured does not, and passes `None`). `bytes` is the
/// resulting resource's size where cheaply known (omitted for a
/// directory delete, where the caller has one recursive walk it already
/// paid for and this is not worth a second).
pub fn file_synced(owner: Option<&str>, path: &str, op: FileOp, bytes: Option<u64>) {
    emit(
        "bread.cloud.file.synced",
        json!({
            "owner": owner,
            "path": path,
            "op": op.as_str(),
            "bytes": bytes,
        }),
    );
}

/// Publish `bread.cloud.sync.conflict` for a write rejected by an
/// `If-Match`/`If-None-Match` precondition: exactly the case a real sync
/// client (mirall/nextcloudcmd) surfaces to the user as a "conflicted
/// copy" instead of silently overwriting or losing a change.
pub fn sync_conflict(owner: Option<&str>, path: &str, reason: ConflictReason) {
    emit(
        "bread.cloud.sync.conflict",
        json!({
            "owner": owner,
            "path": path,
            "reason": reason.as_str(),
        }),
    );
}

/// Publish `bread.cloud.quota.warning` for an account's usage crossing an
/// 80/90/100 percent-of-quota threshold on a write (CLOUD-11). `percent`
/// is the threshold crossed (80, 90, or 100), not the exact usage
/// percentage at the time: see [`crossed_threshold`], which is what a
/// caller should use to decide whether to call this at all.
pub fn quota_warning(owner: Option<&str>, used_bytes: u64, limit_bytes: u64, percent: u8) {
    emit(
        "bread.cloud.quota.warning",
        json!({
            "owner": owner,
            "used_bytes": used_bytes,
            "limit_bytes": limit_bytes,
            "percent": percent,
        }),
    );
}

/// The highest of the 80/90/100 percent-of-`limit_bytes` thresholds that
/// `used_after` reaches or exceeds but `used_before` had not yet reached,
/// or `None` if this write crossed no new threshold.
///
/// Pure and independent of breadd, exercised directly by this module's
/// own tests. Meant to be called from a quota-gated write path
/// (`breadcloud-webdav::handlers::put` and `::uploads::finalize`, both of
/// which already compute a before/after usage pair via
/// `breadcloud_storage::Storage::quota_gate` / `check_quota`) so
/// detecting a crossing costs no extra I/O of its own.
///
/// `used_after` may exceed `limit_bytes`: that is the write that got
/// rejected for being over quota. It is still a legitimate "hit 100%"
/// crossing, since it is exactly the write attempt that surfaces the
/// account being full to whatever is watching `bread.cloud.quota.warning`.
pub fn crossed_threshold(used_before: u64, used_after: u64, limit_bytes: u64) -> Option<u8> {
    if limit_bytes == 0 {
        return None;
    }
    const THRESHOLDS: [u8; 3] = [80, 90, 100];
    let percent_of = |used: u64| -> u64 { used.saturating_mul(100) / limit_bytes };
    let before = percent_of(used_before);
    let after = percent_of(used_after);
    THRESHOLDS
        .into_iter()
        .filter(|&t| before < u64::from(t) && after >= u64::from(t))
        .max()
}

/// Fire-and-forget publish under this app's namespace, dispatched onto
/// Tokio's blocking thread pool. See the module docs for why: the
/// underlying socket write is synchronous and must never share a thread
/// with request handling. The returned `JoinHandle` is intentionally
/// dropped: there is no result to act on and nothing to await.
fn emit(event: &'static str, data: Value) {
    tokio::task::spawn_blocking(move || {
        BreadClient::connect(APP_ID).emit(event, data);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossed_threshold_detects_a_fresh_crossing() {
        assert_eq!(crossed_threshold(70, 85, 100), Some(80));
    }

    #[test]
    fn crossed_threshold_reports_the_highest_threshold_crossed_in_one_jump() {
        // A single large write can jump straight past 80 and 90 at once.
        assert_eq!(crossed_threshold(70, 95, 100), Some(90));
    }

    #[test]
    fn crossed_threshold_is_none_when_still_under_every_threshold() {
        assert_eq!(crossed_threshold(10, 50, 100), None);
    }

    #[test]
    fn crossed_threshold_is_none_when_already_past_before_this_write() {
        // Already at 85%: staying within 80-89% crosses nothing new.
        assert_eq!(crossed_threshold(85, 88, 100), None);
    }

    #[test]
    fn crossed_threshold_treats_an_over_quota_write_as_hitting_100() {
        // The rejected write itself: used_after exceeds limit_bytes.
        assert_eq!(crossed_threshold(95, 110, 100), Some(100));
    }

    #[test]
    fn crossed_threshold_exact_boundary_counts_as_crossing() {
        // Landing exactly on a threshold (not over it) still counts.
        assert_eq!(crossed_threshold(75, 80, 100), Some(80));
    }

    #[test]
    fn crossed_threshold_ignores_a_zero_limit() {
        assert_eq!(crossed_threshold(0, 10, 0), None);
    }

    #[test]
    fn file_op_and_conflict_reason_render_expected_strings() {
        assert_eq!(FileOp::Put.as_str(), "put");
        assert_eq!(FileOp::ChunkedUpload.as_str(), "chunked_upload");
        assert_eq!(FileOp::Delete.as_str(), "delete");
        assert_eq!(FileOp::Move.as_str(), "move");
        assert_eq!(ConflictReason::IfMatch.as_str(), "if-match");
        assert_eq!(ConflictReason::IfNoneMatch.as_str(), "if-none-match");
    }
}
