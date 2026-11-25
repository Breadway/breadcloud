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
//! This integration is explicitly optional (see docs/DESIGN.md):
//! breadcloud must behave identically whether or not `breadd` is
//! installed or running. Concretely:
//!
//! - Publishes use a single bounded queue and dedicated worker. The worker
//!   keeps one IPC connection open while breadd is healthy and reconnects
//!   after a failed write or reply. The request path only tries to enqueue;
//!   it never waits for a socket, and events are dropped when the queue is full.
//! - Nothing here returns a `Result` a caller could feel obligated to
//!   check: there is nothing meaningful to do with a failure to notify
//!   an optional automation bus, and building error plumbing for it
//!   would misrepresent how load-bearing this path is (not at all).
//!
//! Event vocabulary (see docs/DESIGN.md's own "bread ecosystem
//! integration" section for the full catalogue and each field's exact
//! meaning): `bread.cloud.file.synced` (a completed PUT, chunked-upload
//! finalize, DELETE, or MOVE), `bread.cloud.sync.conflict` (an
//! `If-Match`/`If-None-Match` precondition rejection, CLOUD-3), and
//! `bread.cloud.quota.warning` (usage crossing an 80/90/100 percent
//! threshold of a per-account quota, CLOUD-11).

use bread_shared::apps::validate_app_namespace;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{self, SyncSender};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

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

const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Debug)]
struct QueuedEvent {
    name: &'static str,
    data: Value,
}

static EVENT_SENDER: OnceLock<Option<SyncSender<QueuedEvent>>> = OnceLock::new();

/// Fire-and-forget publish under this app's namespace. A bounded queue keeps
/// bursts from creating an unbounded number of blocking operations. If the worker
/// is behind, this event is dropped rather than delaying the caller.
fn emit(event: &'static str, data: Value) {
    let sender = EVENT_SENDER.get_or_init(start_event_worker);
    let Some(sender) = sender else {
        return;
    };
    enqueue_event(sender, QueuedEvent { name: event, data });
}

fn enqueue_event(sender: &SyncSender<QueuedEvent>, event: QueuedEvent) -> bool {
    sender.try_send(event).is_ok()
}

fn start_event_worker() -> Option<SyncSender<QueuedEvent>> {
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
    thread::Builder::new()
        .name("breadcloud-events".to_string())
        .spawn(move || event_worker(receiver))
        .ok()?;
    Some(sender)
}

fn event_worker(receiver: Receiver<QueuedEvent>) {
    let mut client = PersistentEventClient::default();
    while let Ok(event) = receiver.recv() {
        client.emit(event);
    }
}

#[derive(Default)]
struct PersistentEventClient {
    connection: Option<(UnixStream, BufReader<UnixStream>)>,
}

impl PersistentEventClient {
    fn emit(&mut self, event: QueuedEvent) {
        self.emit_with_socket(event, &bread_shared::resolve_socket_path());
    }

    fn emit_with_socket(&mut self, event: QueuedEvent, socket_path: &std::path::Path) {
        if !validate_app_namespace(APP_ID, event.name) {
            return;
        }

        if self.connection.is_none() {
            self.connection = UnixStream::connect(socket_path).ok().and_then(|stream| {
                stream
                    .set_write_timeout(Some(Duration::from_millis(200)))
                    .ok()?;
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .ok()?;
                let reader = BufReader::new(stream.try_clone().ok()?);
                Some((stream, reader))
            });
        }

        let Some((stream, reader)) = self.connection.as_mut() else {
            return;
        };

        let request = json!({
            "id": "0",
            "method": "emit",
            "params": {
                "event": event.name,
                "source": APP_ID,
                "kind": event.name,
                "data": event.data,
            }
        });
        let Ok(line) = serde_json::to_string(&request) else {
            return;
        };

        let result = (|| {
            writeln!(stream, "{line}")?;
            let mut response = String::new();
            reader.read_line(&mut response)?;
            if response.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "breadd closed the event connection",
                ));
            }
            Ok(())
        })();
        if result.is_err() {
            self.connection = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(0);

    fn test_socket_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "breadcloud-events-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn queue_drops_new_events_when_full() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let event = || QueuedEvent {
            name: "bread.cloud.test",
            data: json!({}),
        };

        assert!(enqueue_event(&sender, event()));
        assert!(!enqueue_event(&sender, event()));
    }

    #[test]
    fn client_reuses_one_connection_for_multiple_events() {
        let path = test_socket_path();
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // Some sandboxes forbid creating Unix-domain sockets.
                return;
            }
            Err(error) => panic!("could not bind test socket: {error}"),
        };
        let server_path = path.clone();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut requests = Vec::new();
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                requests.push(serde_json::from_str::<Value>(&line).unwrap());
                writeln!(reader.get_mut(), r#"{{"id":"0","result":null}}"#).unwrap();
            }
            requests
        });

        let mut client = PersistentEventClient { connection: None };
        for name in ["bread.cloud.first", "bread.cloud.second"] {
            client.emit_with_socket(
                QueuedEvent {
                    name,
                    data: json!({}),
                },
                &path,
            );
        }

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["params"]["event"], "bread.cloud.first");
        assert_eq!(requests[1]["params"]["event"], "bread.cloud.second");
        assert!(client.connection.is_some());
        drop(client);
        std::fs::remove_file(server_path).unwrap();
    }

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
