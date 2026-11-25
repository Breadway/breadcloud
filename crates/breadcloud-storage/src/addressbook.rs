//! Addressbook storage (CLOUD-7): addressbooks as named collections of
//! vCard contact objects. Structurally this is the same shape as
//! [`crate::calendar::CalendarStore`] -- a collection of small, opaque,
//! UID-addressed text objects, with a `getctag`/sync-collection change
//! log alongside it -- because CardDAV (RFC 6352) is, like CalDAV (RFC
//! 4791), a WebDAV extension for exactly that shape (vCard objects
//! instead of iCalendar ones). This module is a close structural mirror
//! of `calendar`, not a fresh design: see docs/DESIGN.md's
//! "Shared abstraction, or duplication?" note (in the breadcloud-carddav
//! section) for why this crate does not go a step further and factor the
//! two into one generic "collection-of-typed-objects" store.
//!
//! Layout: one directory per `(user, addressbook id)` pair, each holding
//! a `contacts/` subdirectory that is itself an ordinary [`crate::Storage`]
//! root (one `.vcf` file per contact, named `<uid>.vcf`) plus small
//! sidecar bookkeeping files kept *outside* `contacts/`, the same
//! "reserved sibling, not a dotfile inside the served tree" pattern
//! `calendar` and the file-tree stores (`VersionStore`/`TrashStore`) all
//! use:
//!
//! ```text
//! <addressbook root>/<user>/<addressbook id>/
//!   display-name       human-readable addressbook name
//!   ctag               current change-tag: a plain decimal counter,
//!                      bumped on every contact create/update/delete
//!   changelog          append-only change log backing sync-collection
//!                      (RFC 6578): one line per change, "<seq>\t<uid>\t<C|D>"
//!   contacts/          a plain `Storage` root: "<uid>.vcf" per contact
//! ```
//!
//! **Contact groups are not a separate concept here.** The real data this
//! issue targets (76 contacts across "Friends"/"ICE"/"starred"/"Family
//! team" groups) maps groups onto ordinary vCard resources with
//! `KIND:group` and `MEMBER:urn:uuid:<contact uid>` properties, the
//! standard CardDAV/vCard 4 representation both DAVx5 and iOS Contacts
//! understand -- see `breadcloud_carddav::vcard`'s doc comment. A group is
//! just another `.vcf` object in the same `contacts/` directory as the
//! people it lists; this store has no separate storage path, API, or
//! sidecar for it.
//!
//! **One addressbook is what this deployment actually needs** (the real
//! data is 76 contacts in one set of groups, not several independent
//! addressbooks the way the calendar issue's real data was genuinely
//! split across personal/family/school), but the store itself supports
//! more than one per user, the same way `CalendarStore` does, for two
//! reasons: it costs almost nothing on top of the structural mirror this
//! module already is, and `MKCOL` (the CardDAV addressbook-creation verb,
//! see `breadcloud_carddav::handlers::mkcol`) needs somewhere to create a
//! second addressbook if a client or a human ever asks for one, the same
//! way `MKCALENDAR` does for calendars. `breadcloud-server` only
//! bootstraps a single `contacts` addressbook per user by default.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use tokio::fs;
use tokio::sync::Mutex as AsyncMutex;

use crate::calendar::ChangeEntry;
use crate::error::StorageError;
use crate::path::DavPath;
use crate::Storage;

/// One addressbook collection's metadata.
#[derive(Debug, Clone)]
pub struct AddressbookInfo {
    pub id: String,
    pub display_name: String,
    /// Current change-tag: opaque to clients, but guaranteed to change
    /// whenever any contact in this addressbook is created, updated, or
    /// deleted. Formatted as a plain decimal counter -- same convention
    /// as `CalendarInfo::ctag`.
    pub ctag: String,
}

/// Addressbook storage, sandboxed to its own root directory, distinct
/// from the served file tree, chunked-upload staging, version history,
/// trash, and calendar storage.
#[derive(Debug, Clone)]
pub struct AddressbookStore {
    root: PathBuf,
    write_lock: Arc<AsyncMutex<()>>,
    resource_locks: Arc<AsyncMutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>>,
}

impl AddressbookStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        AddressbookStore {
            root: root.into(),
            write_lock: Arc::new(AsyncMutex::new(())),
            resource_locks: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Serialize one resource's conditional mutation across requests and
    /// store clones, including change recording. Weak entries keep the map
    /// bounded by active resources rather than every resource ever written.
    pub async fn resource_gate(
        &self,
        user: &str,
        addressbook_id: &str,
        object: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, StorageError> {
        let user = Self::validate_segment(user)?;
        let collection = Self::validate_segment(addressbook_id)?;
        let object = Self::validate_segment(object)?;
        let key = self.root.join(user).join(collection).join(object);
        let lock = {
            let mut locks = self.resource_locks.lock().await;
            locks.retain(|_, value| value.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        Ok(lock.lock_owned().await)
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Same segment validation `CalendarStore` uses -- reused via
    /// `DavPath::parse` rather than reimplemented, so a username or
    /// addressbook id coming straight off a URL segment gets the exact
    /// same `..`/NUL/empty rejection a file path would.
    fn validate_segment(raw: &str) -> Result<String, StorageError> {
        let parsed = DavPath::parse(&format!("/{raw}"))?;
        match parsed.segments() {
            [only] if only == raw => Ok(only.clone()),
            _ => Err(StorageError::InvalidPath(crate::path::InvalidPath(
                raw.to_string(),
            ))),
        }
    }

    fn addressbook_dir(&self, user: &str, addressbook_id: &str) -> PathBuf {
        self.root.join(user).join(addressbook_id)
    }

    fn contacts_dir(&self, user: &str, addressbook_id: &str) -> PathBuf {
        self.addressbook_dir(user, addressbook_id).join("contacts")
    }

    fn display_name_file(&self, user: &str, addressbook_id: &str) -> PathBuf {
        self.addressbook_dir(user, addressbook_id)
            .join("display-name")
    }

    fn ctag_file(&self, user: &str, addressbook_id: &str) -> PathBuf {
        self.addressbook_dir(user, addressbook_id).join("ctag")
    }

    fn changelog_file(&self, user: &str, addressbook_id: &str) -> PathBuf {
        self.addressbook_dir(user, addressbook_id).join("changelog")
    }

    /// A `Storage` rooted at this addressbook's `contacts/` directory, so
    /// the CardDAV HTTP layer can PUT/GET/DELETE/list `.vcf` files with
    /// `Storage`'s existing atomic-write and content-etag machinery.
    /// `NotFound` if the addressbook itself does not exist -- callers
    /// that want a clearer "no such addressbook" error should check
    /// [`AddressbookStore::addressbook_exists`] first.
    pub async fn contacts_storage(
        &self,
        user: &str,
        addressbook_id: &str,
    ) -> Result<Storage, StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;
        let dir = self.contacts_dir(&user, &addressbook_id);
        if fs::metadata(&dir).await.is_err() {
            return Err(StorageError::NotFound);
        }
        Ok(Storage::new(dir))
    }

    pub async fn addressbook_exists(&self, user: &str, addressbook_id: &str) -> bool {
        let (Ok(user), Ok(addressbook_id)) = (
            Self::validate_segment(user),
            Self::validate_segment(addressbook_id),
        ) else {
            return false;
        };
        fs::metadata(self.addressbook_dir(&user, &addressbook_id))
            .await
            .is_ok()
    }

    /// Create a new addressbook collection (`MKCOL`, RFC 6352 does not
    /// have its own creation verb the way `MKCALENDAR` does -- see
    /// `breadcloud_carddav::handlers::mkcol`). `AlreadyExists` if
    /// `addressbook_id` is already taken for this user.
    pub async fn create_addressbook(
        &self,
        user: &str,
        addressbook_id: &str,
        display_name: &str,
    ) -> Result<AddressbookInfo, StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;

        let _guard = self.write_lock.lock().await;

        let dir = self.addressbook_dir(&user, &addressbook_id);
        if fs::metadata(&dir).await.is_ok() {
            return Err(StorageError::AlreadyExists);
        }

        fs::create_dir_all(self.contacts_dir(&user, &addressbook_id)).await?;
        fs::write(self.display_name_file(&user, &addressbook_id), display_name).await?;
        fs::write(self.ctag_file(&user, &addressbook_id), "0").await?;
        fs::write(self.changelog_file(&user, &addressbook_id), "").await?;

        Ok(AddressbookInfo {
            id: addressbook_id,
            display_name: display_name.to_string(),
            ctag: "0".to_string(),
        })
    }

    /// Idempotent variant of [`AddressbookStore::create_addressbook`] for
    /// server startup bootstrapping the default `contacts` addressbook:
    /// does nothing (not an error) if it already exists.
    pub async fn ensure_addressbook(
        &self,
        user: &str,
        addressbook_id: &str,
        display_name: &str,
    ) -> Result<(), StorageError> {
        match self
            .create_addressbook(user, addressbook_id, display_name)
            .await
        {
            Ok(_) | Err(StorageError::AlreadyExists) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn get_addressbook(
        &self,
        user: &str,
        addressbook_id: &str,
    ) -> Result<AddressbookInfo, StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;
        if fs::metadata(self.addressbook_dir(&user, &addressbook_id))
            .await
            .is_err()
        {
            return Err(StorageError::NotFound);
        }
        let display_name = fs::read_to_string(self.display_name_file(&user, &addressbook_id))
            .await
            .unwrap_or_default();
        let ctag = self.ctag(&user, &addressbook_id).await?;
        Ok(AddressbookInfo {
            id: addressbook_id,
            display_name: display_name.trim().to_string(),
            ctag,
        })
    }

    /// List every addressbook belonging to `user`. Empty (not an error)
    /// if the user has no addressbooks directory yet.
    pub async fn list_addressbooks(
        &self,
        user: &str,
    ) -> Result<Vec<AddressbookInfo>, StorageError> {
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
            out.push(self.get_addressbook(&user, &id).await?);
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    pub async fn ctag(&self, user: &str, addressbook_id: &str) -> Result<String, StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;
        let raw = fs::read_to_string(self.ctag_file(&user, &addressbook_id))
            .await
            .unwrap_or_else(|_| "0".to_string());
        Ok(raw.trim().to_string())
    }

    /// Record that `uid` was created/updated (`deleted = false`) or
    /// removed (`deleted = true`) in this addressbook, bump the ctag, and
    /// return the new ctag. Callers (the PUT and DELETE contact handlers)
    /// call this after the underlying `contacts_storage` write/delete has
    /// already landed, mirroring `CalendarStore::record_change` exactly.
    pub async fn record_change(
        &self,
        user: &str,
        addressbook_id: &str,
        uid: &str,
        deleted: bool,
    ) -> Result<String, StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;
        if uid.chars().any(char::is_control) {
            return Err(crate::path::InvalidPath(uid.to_string()).into());
        }

        let _guard = self.write_lock.lock().await;

        let current: u64 = fs::read_to_string(self.ctag_file(&user, &addressbook_id))
            .await
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let next = current + 1;

        // Guarded by write_lock (held for this whole function), same
        // reasoning as CalendarStore::record_change: a plain append is
        // safe here, no concurrent writer to race with.
        let line = format!("{next}\t{}\t{uid}\n", if deleted { "D" } else { "C" });
        use tokio::io::AsyncWriteExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.changelog_file(&user, &addressbook_id))
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;

        let ctag = next.to_string();
        crate::write_file_atomically(&self.ctag_file(&user, &addressbook_id), ctag.as_bytes())
            .await?;
        Ok(ctag)
    }

    /// The changes recorded since `since` (exclusive), deduped to each
    /// `uid`'s latest entry in that range, plus the current ctag --
    /// identical semantics to `CalendarStore::changes_since` (see that
    /// method's doc comment for the initial-sync tombstone-dropping
    /// caveat callers need to apply). Reuses [`ChangeEntry`] (from
    /// `crate::calendar`) directly rather than a new, identically-shaped
    /// `ContactChangeEntry`: both are just "a uid, and whether it was
    /// deleted", and the object the uid names (an event vs. a contact) is
    /// not part of the shape sync-collection diffing actually needs.
    pub async fn changes_since(
        &self,
        user: &str,
        addressbook_id: &str,
        since: Option<u64>,
    ) -> Result<(Vec<ChangeEntry>, String), StorageError> {
        let user = Self::validate_segment(user)?;
        let addressbook_id = Self::validate_segment(addressbook_id)?;

        let ctag = self.ctag(&user, &addressbook_id).await?;
        let raw = fs::read_to_string(self.changelog_file(&user, &addressbook_id))
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

    /// Parse a sync-token this store previously handed out, same format
    /// and validation behavior as `CalendarStore::parse_sync_token`.
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

    fn store() -> (TempDir, AddressbookStore) {
        let dir = TempDir::new().unwrap();
        let store = AddressbookStore::new(dir.path());
        (dir, store)
    }

    #[tokio::test]
    async fn create_list_and_get_addressbook() {
        let (_dir, store) = store();
        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        store
            .create_addressbook("dev", "work", "Work")
            .await
            .unwrap();

        let books = store.list_addressbooks("dev").await.unwrap();
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].id, "contacts");
        assert_eq!(books[1].id, "work");
        assert_eq!(books[0].ctag, "0");
    }

    #[tokio::test]
    async fn creating_a_duplicate_addressbook_id_fails() {
        let (_dir, store) = store();
        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        let err = store
            .create_addressbook("dev", "contacts", "Contacts Again")
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists));
    }

    #[tokio::test]
    async fn ensure_addressbook_is_idempotent() {
        let (_dir, store) = store();
        store
            .ensure_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        store
            .ensure_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        assert_eq!(store.list_addressbooks("dev").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn nonexistent_user_has_no_addressbooks() {
        let (_dir, store) = store();
        assert!(store.list_addressbooks("nobody").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn contacts_storage_requires_an_existing_addressbook() {
        let (_dir, store) = store();
        let err = store.contacts_storage("dev", "contacts").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound));

        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        let contacts = store.contacts_storage("dev", "contacts").await.unwrap();
        let uid_path = DavPath::parse("/contact-1.vcf").unwrap();
        contacts
            .write_bytes(&uid_path, b"BEGIN:VCARD")
            .await
            .unwrap();
        assert!(contacts.exists(&uid_path).await);
    }

    #[tokio::test]
    async fn addressbook_id_with_traversal_is_rejected() {
        let (_dir, store) = store();
        let err = store
            .create_addressbook("dev", "../escape", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidPath(_)));
    }

    #[tokio::test]
    async fn ctag_bumps_on_every_recorded_change() {
        let (_dir, store) = store();
        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        assert_eq!(store.ctag("dev", "contacts").await.unwrap(), "0");

        let ctag1 = store
            .record_change("dev", "contacts", "uid-1", false)
            .await
            .unwrap();
        assert_eq!(ctag1, "1");
        let ctag2 = store
            .record_change("dev", "contacts", "uid-2", false)
            .await
            .unwrap();
        assert_eq!(ctag2, "2");
        assert_eq!(store.ctag("dev", "contacts").await.unwrap(), "2");
    }

    #[tokio::test]
    async fn record_change_rejects_control_characters_in_uid() {
        let (_dir, store) = store();
        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();

        let err = store
            .record_change("dev", "contacts", "bad\nuid", false)
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::InvalidPath(_)));
        assert_eq!(store.ctag("dev", "contacts").await.unwrap(), "0");

        store
            .record_change("dev", "contacts", "good-uid", false)
            .await
            .unwrap();
        let log = fs::read_to_string(store.changelog_file("dev", "contacts"))
            .await
            .unwrap();
        assert_eq!(log, "1\tC\tgood-uid\n");
    }

    #[tokio::test]
    async fn changes_since_dedupes_to_latest_state_per_uid() {
        let (_dir, store) = store();
        store
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        store
            .record_change("dev", "contacts", "uid-1", false)
            .await
            .unwrap();
        store
            .record_change("dev", "contacts", "uid-2", false)
            .await
            .unwrap();
        store
            .record_change("dev", "contacts", "uid-1", true)
            .await
            .unwrap();

        let (changes, ctag) = store.changes_since("dev", "contacts", None).await.unwrap();
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
            .create_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        store
            .record_change("dev", "contacts", "uid-1", false)
            .await
            .unwrap();
        let (_, ctag_after_first) = store.changes_since("dev", "contacts", None).await.unwrap();
        store
            .record_change("dev", "contacts", "uid-2", false)
            .await
            .unwrap();

        let since = AddressbookStore::parse_sync_token(&ctag_after_first).unwrap();
        let (changes, _) = store
            .changes_since("dev", "contacts", Some(since))
            .await
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].uid, "uid-2");
    }

    #[test]
    fn sync_token_roundtrip() {
        let token = AddressbookStore::format_sync_token("42");
        assert_eq!(AddressbookStore::parse_sync_token(&token), Some(42));
        assert_eq!(AddressbookStore::parse_sync_token("42"), Some(42));
        assert_eq!(AddressbookStore::parse_sync_token("not-a-number"), None);
    }
}
