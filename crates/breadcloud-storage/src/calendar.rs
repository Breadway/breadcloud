//! Calendar storage (CLOUD-6): calendars as named collections of
//! iCalendar event objects, distinct from the file [`crate::Storage`]
//! root the same way [`crate::VersionStore`] and [`crate::TrashStore`]
//! already are -- an event is not a file in the served WebDAV tree, so it
//! gets its own sandboxed concept rather than being force-fit into
//! `Storage`'s path-addressed model.
//!
//! Layout: one directory per `(user, calendar id)` pair, each holding an
//! `events/` subdirectory that is itself an ordinary [`crate::Storage`]
//! root (one `.ics` file per event, named `<uid>.ics`) plus small sidecar
//! bookkeeping files kept *outside* `events/` so a listing of the
//! calendar's events (`events_storage(..).list_dir`) never sees them --
//! the same "reserved sibling, not a dotfile inside the served tree"
//! pattern `AppState::uploads`/`VersionStore`/`TrashStore` all use for
//! their own out-of-band data:
//!
//! ```text
//! <calendar root>/<user>/<calendar id>/
//!   display-name       human-readable calendar name
//!   color              optional iCalendar color hint (may be absent)
//!   ctag               current change-tag: a plain decimal counter,
//!                      bumped on every event create/update/delete
//!   changelog          append-only change log backing sync-collection
//!                      (RFC 6578): one line per change, "<seq>\t<uid>\t<C|D>"
//!   events/            a plain `Storage` root: "<uid>.ics" per event
//! ```
//!
//! Reusing `Storage` for `events/` rather than writing a second
//! content-addressed file store from scratch gets calendar objects the
//! same atomic (temp-file-then-rename) writes and the same
//! content-derived, cached blake3 etags `Storage` already gives file
//! resources -- exactly the property CalDAV's own `getetag` needs, for
//! free.
//!
//! **ctag and sync-collection.** `getctag` (the de facto "has anything in
//! this collection changed" property every real CalDAV client polls) is
//! just the current value of the `ctag` counter, so checking it is an
//! O(1) file read. `sync-collection` (RFC 6578) replays `changelog`
//! entries newer than the sync-token a client presents, deduped to each
//! `uid`'s *latest* entry in that range (an event created then deleted
//! within one window collapses to "deleted"; several updates collapse to
//! one "changed"), which is exactly the "tell me only the current diff"
//! semantics the REPORT needs, not a call-by-call replay history.
//!
//! **Concurrency.** A single `tokio::sync::Mutex` guards every mutating
//! operation across the *whole* store (not just one calendar). At this
//! deployment's real scale -- three calendars, a few hundred events
//! total, one interactive user -- that is not a contended bottleneck, and
//! it sidesteps an entire class of sequence-number races a per-calendar
//! lock map would otherwise need careful design to rule out (two events
//! landing in the same calendar at once must never be assigned the same
//! `ctag` bump). Reads (list/get/ctag/changes_since) are not gated by the
//! lock: they only ever observe a filesystem state some completed write
//! left behind, never a partial one, because every write below only
//! publishes its result via a rename or a single small `fs::write` of a
//! file nothing else reads mid-write.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::fs;
use tokio::sync::Mutex as AsyncMutex;

use crate::error::StorageError;
use crate::path::DavPath;
use crate::Storage;

/// One calendar collection's metadata.
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    pub id: String,
    pub display_name: String,
    pub color: Option<String>,
    /// Current change-tag: opaque to clients, but guaranteed to change
    /// whenever any event in this calendar is created, updated, or
    /// deleted. Formatted as a plain decimal counter.
    pub ctag: String,
}

/// One entry in a `sync-collection` REPORT's diff: either the resource
/// changed (client should re-fetch it) or it was removed (client should
/// treat it as a 404 / drop it locally).
#[derive(Debug, Clone)]
pub struct ChangeEntry {
    pub uid: String,
    pub deleted: bool,
}

/// Calendar storage, sandboxed to its own root directory, distinct from
/// the served file tree, chunked-upload staging, version history, and
/// trash.
#[derive(Debug, Clone)]
pub struct CalendarStore {
    root: PathBuf,
    write_lock: Arc<AsyncMutex<()>>,
}

impl CalendarStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        CalendarStore {
            root: root.into(),
            write_lock: Arc::new(AsyncMutex::new(())),
        }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Validate a single path segment (a username or calendar id) the
    /// same way `DavPath::parse` validates a whole request path -- reused
    /// here rather than reimplemented, so a calendar id coming straight
    /// off a URL segment gets the exact same `..`/NUL/empty rejection a
    /// file path would, instead of a second, easier-to-miss copy of that
    /// logic living in this module.
    fn validate_segment(raw: &str) -> Result<String, StorageError> {
        let parsed = DavPath::parse(&format!("/{raw}"))?;
        match parsed.segments() {
            [only] if only == raw => Ok(only.clone()),
            _ => Err(StorageError::InvalidPath(crate::path::InvalidPath(
                raw.to_string(),
            ))),
        }
    }

    fn calendar_dir(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.root.join(user).join(calendar_id)
    }

    fn events_dir(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.calendar_dir(user, calendar_id).join("events")
    }

    fn display_name_file(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.calendar_dir(user, calendar_id).join("display-name")
    }

    fn color_file(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.calendar_dir(user, calendar_id).join("color")
    }

    fn ctag_file(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.calendar_dir(user, calendar_id).join("ctag")
    }

    fn changelog_file(&self, user: &str, calendar_id: &str) -> PathBuf {
        self.calendar_dir(user, calendar_id).join("changelog")
    }

    /// A `Storage` rooted at this calendar's `events/` directory, so the
    /// caller (the CalDAV HTTP layer) can PUT/GET/DELETE/list `.ics`
    /// files with `Storage`'s existing atomic-write and content-etag
    /// machinery rather than this crate reimplementing it. Returns
    /// `NotFound` if the calendar itself does not exist -- callers that
    /// want a clearer "no such calendar" error should check
    /// [`CalendarStore::calendar_exists`] first.
    pub async fn events_storage(
        &self,
        user: &str,
        calendar_id: &str,
    ) -> Result<Storage, StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;
        let dir = self.events_dir(&user, &calendar_id);
        if fs::metadata(&dir).await.is_err() {
            return Err(StorageError::NotFound);
        }
        Ok(Storage::new(dir))
    }

    pub async fn calendar_exists(&self, user: &str, calendar_id: &str) -> bool {
        let (Ok(user), Ok(calendar_id)) = (
            Self::validate_segment(user),
            Self::validate_segment(calendar_id),
        ) else {
            return false;
        };
        fs::metadata(self.calendar_dir(&user, &calendar_id))
            .await
            .is_ok()
    }

    /// Create a new calendar collection (MKCALENDAR). `AlreadyExists` if
    /// `calendar_id` is already taken for this user.
    pub async fn create_calendar(
        &self,
        user: &str,
        calendar_id: &str,
        display_name: &str,
        color: Option<&str>,
    ) -> Result<CalendarInfo, StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;

        let _guard = self.write_lock.lock().await;

        let dir = self.calendar_dir(&user, &calendar_id);
        if fs::metadata(&dir).await.is_ok() {
            return Err(StorageError::AlreadyExists);
        }

        fs::create_dir_all(self.events_dir(&user, &calendar_id)).await?;
        fs::write(self.display_name_file(&user, &calendar_id), display_name).await?;
        if let Some(color) = color {
            fs::write(self.color_file(&user, &calendar_id), color).await?;
        }
        fs::write(self.ctag_file(&user, &calendar_id), "0").await?;
        fs::write(self.changelog_file(&user, &calendar_id), "").await?;

        Ok(CalendarInfo {
            id: calendar_id,
            display_name: display_name.to_string(),
            color: color.map(str::to_string),
            ctag: "0".to_string(),
        })
    }

    /// Idempotent variant of [`CalendarStore::create_calendar`] for
    /// server startup bootstrapping the baseline calendar set (personal /
    /// family / school): does nothing (not an error) if the calendar
    /// already exists.
    pub async fn ensure_calendar(
        &self,
        user: &str,
        calendar_id: &str,
        display_name: &str,
        color: Option<&str>,
    ) -> Result<(), StorageError> {
        match self
            .create_calendar(user, calendar_id, display_name, color)
            .await
        {
            Ok(_) | Err(StorageError::AlreadyExists) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn get_calendar(
        &self,
        user: &str,
        calendar_id: &str,
    ) -> Result<CalendarInfo, StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;
        if fs::metadata(self.calendar_dir(&user, &calendar_id))
            .await
            .is_err()
        {
            return Err(StorageError::NotFound);
        }
        let display_name = fs::read_to_string(self.display_name_file(&user, &calendar_id))
            .await
            .unwrap_or_default();
        let color = fs::read_to_string(self.color_file(&user, &calendar_id))
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let ctag = self.ctag(&user, &calendar_id).await?;
        Ok(CalendarInfo {
            id: calendar_id,
            display_name: display_name.trim().to_string(),
            color,
            ctag,
        })
    }

    /// List every calendar belonging to `user`. Empty (not an error) if
    /// the user has no calendars directory yet.
    pub async fn list_calendars(&self, user: &str) -> Result<Vec<CalendarInfo>, StorageError> {
        let user = Self::validate_segment(user)?;
        let user_dir = self.root.join(&user);
        let mut rd = match fs::read_dir(&user_dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if !entry.metadata().await?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            out.push(self.get_calendar(&user, &id).await?);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub async fn ctag(&self, user: &str, calendar_id: &str) -> Result<String, StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;
        let raw = fs::read_to_string(self.ctag_file(&user, &calendar_id))
            .await
            .unwrap_or_else(|_| "0".to_string());
        Ok(raw.trim().to_string())
    }

    /// Record that `uid` was created/updated (`deleted = false`) or
    /// removed (`deleted = true`) in this calendar, bump the ctag, and
    /// return the new ctag. Callers (the PUT and DELETE event handlers)
    /// call this after the underlying `events_storage` write/delete has
    /// already landed, so the changelog never claims a change that did
    /// not actually happen.
    pub async fn record_change(
        &self,
        user: &str,
        calendar_id: &str,
        uid: &str,
        deleted: bool,
    ) -> Result<String, StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;

        let _guard = self.write_lock.lock().await;

        let current: u64 = fs::read_to_string(self.ctag_file(&user, &calendar_id))
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;

        // Guarded by write_lock (held for this whole function): there is
        // no concurrent writer to this changelog, so a plain append
        // (rather than a staged temp-file swap) is safe -- the only
        // failure mode is a partial last line on an ungraceful process
        // kill mid-append, which `changes_since` below tolerates by
        // skipping any line it cannot fully parse.
        let line = format!("{next}\t{}\t{uid}\n", if deleted { "D" } else { "C" });
        use tokio::io::AsyncWriteExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.changelog_file(&user, &calendar_id))
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;

        let ctag = next.to_string();
        fs::write(self.ctag_file(&user, &calendar_id), &ctag).await?;
        Ok(ctag)
    }

    /// The changes recorded since `since` (exclusive), deduped to each
    /// `uid`'s latest entry in that range, plus the current ctag (usable
    /// directly as the new sync-token to hand back to the client). `since
    /// = None` returns every change ever recorded for this calendar
    /// (still deduped to one entry per uid) -- callers doing an *initial*
    /// sync-collection (no token presented yet) additionally need to drop
    /// any entry whose latest state is `deleted`, since a resource the
    /// client never had is not a meaningful deletion to report; see
    /// `breadcloud_caldav::handlers::report`.
    pub async fn changes_since(
        &self,
        user: &str,
        calendar_id: &str,
        since: Option<u64>,
    ) -> Result<(Vec<ChangeEntry>, String), StorageError> {
        let user = Self::validate_segment(user)?;
        let calendar_id = Self::validate_segment(calendar_id)?;

        let ctag = self.ctag(&user, &calendar_id).await?;
        let raw = fs::read_to_string(self.changelog_file(&user, &calendar_id))
            .await
            .unwrap_or_default();

        let floor = since.unwrap_or(0);
        let mut latest: std::collections::BTreeMap<String, (u64, bool)> =
            std::collections::BTreeMap::new();
        for line in raw.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(seq_raw), Some(flag), Some(uid)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(seq) = seq_raw.parse::<u64>() else {
                continue;
            };
            if seq <= floor {
                continue;
            }
            let deleted = flag == "D";
            latest
                .entry(uid.to_string())
                .and_modify(|(existing_seq, existing_deleted)| {
                    if seq > *existing_seq {
                        *existing_seq = seq;
                        *existing_deleted = deleted;
                    }
                })
                .or_insert((seq, deleted));
        }

        let entries = latest
            .into_iter()
            .map(|(uid, (_, deleted))| ChangeEntry { uid, deleted })
            .collect();
        Ok((entries, ctag))
    }

    /// Parse a sync-token this store previously handed out (a plain
    /// decimal ctag value) back into the sequence number
    /// [`CalendarStore::changes_since`] expects. `None` for anything that
    /// is not a plain non-negative integer -- callers treat that as "no
    /// usable token, do a full initial sync" rather than an error, since
    /// RFC 6578 has clients treat an unrecognized/expired token that way
    /// too (a 403 with `valid-sync-token` asking for a fresh
    /// no-sync-token request).
    pub fn parse_sync_token(token: &str) -> Option<u64> {
        token
            .strip_prefix("breadcloud-sync:")
            .unwrap_or(token)
            .trim()
            .parse()
            .ok()
    }

    pub fn format_sync_token(ctag: &str) -> String {
        format!("breadcloud-sync:{ctag}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, CalendarStore) {
        let dir = TempDir::new().unwrap();
        let store = CalendarStore::new(dir.path());
        (dir, store)
    }

    #[tokio::test]
    async fn create_list_and_get_calendar() {
        let (_dir, store) = store();
        store
            .create_calendar("dev", "personal", "Personal", Some("#ff0000"))
            .await
            .unwrap();
        store
            .create_calendar("dev", "family", "Family", None)
            .await
            .unwrap();

        let calendars = store.list_calendars("dev").await.unwrap();
        assert_eq!(calendars.len(), 2);
        assert_eq!(calendars[0].id, "family");
        assert_eq!(calendars[0].color, None);
        assert_eq!(calendars[1].id, "personal");
        assert_eq!(calendars[1].display_name, "Personal");
        assert_eq!(calendars[1].color.as_deref(), Some("#ff0000"));
        assert_eq!(calendars[1].ctag, "0");
    }

    #[tokio::test]
    async fn creating_a_duplicate_calendar_id_fails() {
        let (_dir, store) = store();
        store
            .create_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        let err = store
            .create_calendar("dev", "personal", "Personal Again", None)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists));
    }

    #[tokio::test]
    async fn ensure_calendar_is_idempotent() {
        let (_dir, store) = store();
        store
            .ensure_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        store
            .ensure_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        assert_eq!(store.list_calendars("dev").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn nonexistent_user_has_no_calendars() {
        let (_dir, store) = store();
        assert!(store.list_calendars("nobody").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn events_storage_requires_an_existing_calendar() {
        let (_dir, store) = store();
        let err = store.events_storage("dev", "personal").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound));

        store
            .create_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        let events = store.events_storage("dev", "personal").await.unwrap();
        let uid_path = DavPath::parse("/event-1.ics").unwrap();
        events
            .write_bytes(&uid_path, b"BEGIN:VEVENT")
            .await
            .unwrap();
        assert!(events.exists(&uid_path).await);
    }

    #[tokio::test]
    async fn calendar_id_with_traversal_is_rejected() {
        let (_dir, store) = store();
        let err = store
            .create_calendar("dev", "../escape", "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[tokio::test]
    async fn ctag_bumps_on_every_recorded_change() {
        let (_dir, store) = store();
        store
            .create_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        assert_eq!(store.ctag("dev", "personal").await.unwrap(), "0");

        let ctag1 = store
            .record_change("dev", "personal", "uid-1", false)
            .await
            .unwrap();
        assert_eq!(ctag1, "1");
        let ctag2 = store
            .record_change("dev", "personal", "uid-2", false)
            .await
            .unwrap();
        assert_eq!(ctag2, "2");
        assert_eq!(store.ctag("dev", "personal").await.unwrap(), "2");
    }

    #[tokio::test]
    async fn changes_since_dedupes_to_latest_state_per_uid() {
        let (_dir, store) = store();
        store
            .create_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        store
            .record_change("dev", "personal", "uid-1", false)
            .await
            .unwrap();
        store
            .record_change("dev", "personal", "uid-2", false)
            .await
            .unwrap();
        store
            .record_change("dev", "personal", "uid-1", true)
            .await
            .unwrap();

        let (changes, ctag) = store.changes_since("dev", "personal", None).await.unwrap();
        assert_eq!(ctag, "3");
        assert_eq!(changes.len(), 2);
        let uid1 = changes.iter().find(|c| c.uid == "uid-1").unwrap();
        assert!(uid1.deleted);
        let uid2 = changes.iter().find(|c| c.uid == "uid-2").unwrap();
        assert!(!uid2.deleted);
    }

    #[tokio::test]
    async fn changes_since_a_token_only_returns_newer_entries() {
        let (_dir, store) = store();
        store
            .create_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        store
            .record_change("dev", "personal", "uid-1", false)
            .await
            .unwrap();
        let (_, ctag_after_first) = store.changes_since("dev", "personal", None).await.unwrap();
        store
            .record_change("dev", "personal", "uid-2", false)
            .await
            .unwrap();

        let since = CalendarStore::parse_sync_token(&ctag_after_first).unwrap();
        let (changes, _) = store
            .changes_since("dev", "personal", Some(since))
            .await
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uid, "uid-2");
    }

    #[test]
    fn sync_token_roundtrip() {
        let token = CalendarStore::format_sync_token("42");
        assert_eq!(CalendarStore::parse_sync_token(&token), Some(42));
        assert_eq!(CalendarStore::parse_sync_token("42"), Some(42));
        assert_eq!(CalendarStore::parse_sync_token("not-a-number"), None);
    }
}
