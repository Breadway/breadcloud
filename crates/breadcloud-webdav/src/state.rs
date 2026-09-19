use std::sync::Arc;

use breadcloud_storage::{Storage, TrashStore, VersionStore};

/// Shared state for the whole WebDAV router.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<Storage>,
    /// Staging area for in-progress chunked (NG chunking v2) uploads,
    /// sandboxed to its own root distinct from `storage`. Kept separate
    /// rather than a dotdir under the served tree so a PROPFIND on the
    /// real tree never has a chance of listing someone's in-flight
    /// upload chunks alongside their files.
    pub uploads: Arc<Storage>,
    /// Per-file version history (CLOUD-4), sandboxed to its own root
    /// distinct from both `storage` and `uploads`, for the same reason as
    /// `uploads`: a PROPFIND of the real tree must never see this data.
    /// See `crate::versioning` for where snapshots get taken, and
    /// `crate::handlers::versions` for the minimal DAV surface that
    /// browses it.
    pub versions: Arc<VersionStore>,
    /// Soft-delete trash (CLOUD-5), sandboxed to its own root distinct
    /// from all of the above, same reason again. See `crate::trash` for
    /// where DELETE moves content here instead of removing it, and
    /// `crate::handlers::trashbin` for the DAV surface that lists,
    /// restores, and purges entries.
    pub trash: Arc<TrashStore>,
    pub auth: Option<BasicAuthConfig>,
    /// This account's storage quota in bytes (CLOUD-11). `None` means
    /// unlimited -- the pre-CLOUD-11 default, still what a standalone/test
    /// `AppState` gets unless a caller opts in with
    /// [`AppState::with_quota_limit`]. See `Storage::quota_report` and
    /// `Storage::check_quota`, which are what actually consult this.
    pub quota_limit_bytes: Option<u64>,
    /// The username this already-resolved `AppState` belongs to (CLOUD-11).
    /// Distinct from `auth` (which is "credentials to check on this
    /// crate's own standalone auth gate") -- in the real combined server,
    /// `AppState` is resolved to one specific account *before* any
    /// handler runs (see `breadcloud-server::combined`), and `auth` is
    /// `None` there since authentication happens once, centrally, ahead
    /// of that resolution. Handlers that need to know "whose data is
    /// this" (the OCS `cloud/user` endpoint) read this field, falling
    /// back to `auth`'s username for standalone/test use where `owner`
    /// is never set.
    pub owner: Option<String>,
}

/// Placeholder single-user credentials. CLOUD-11 replaces this with real
/// multi-user auth and per-user quotas; until then this is the only gate
/// on the server.
#[derive(Clone)]
pub struct BasicAuthConfig {
    pub username: String,
    pub password: String,
}

impl AppState {
    pub fn new(
        storage: Storage,
        uploads: Storage,
        versions: VersionStore,
        trash: TrashStore,
    ) -> Self {
        AppState {
            storage: Arc::new(storage),
            uploads: Arc::new(uploads),
            versions: Arc::new(versions),
            trash: Arc::new(trash),
            auth: None,
            quota_limit_bytes: None,
            owner: None,
        }
    }

    /// Require HTTP Basic auth with these credentials on every request.
    /// Without this, the server is open (fine for local development,
    /// never for anything reachable off localhost).
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Some(BasicAuthConfig {
            username: username.into(),
            password: password.into(),
        });
        self
    }

    /// Set this account's storage quota (CLOUD-11). See
    /// `AppState::quota_limit_bytes`.
    pub fn with_quota_limit(mut self, quota_limit_bytes: u64) -> Self {
        self.quota_limit_bytes = Some(quota_limit_bytes);
        self
    }

    /// Mark this already-resolved `AppState` as belonging to `username`
    /// (CLOUD-11). See `AppState::owner`.
    pub fn with_owner(mut self, username: impl Into<String>) -> Self {
        self.owner = Some(username.into());
        self
    }

    /// The account this `AppState` belongs to, for attaching an `owner`
    /// field to a `bread.cloud.*` event (CLOUD-13): `owner` if this
    /// instance is already resolved to one account (the real combined
    /// server, CLOUD-11), falling back to `auth`'s configured username
    /// for a standalone/test instance where `owner` is never set. `None`
    /// (rather than a placeholder string) when neither is available, so
    /// an event's `owner` field is honestly absent instead of a made-up
    /// value. Same fallback `dispatch`'s own OCS `cloud/user` handler
    /// uses (see this crate's `lib.rs`).
    pub fn owner_label(&self) -> Option<&str> {
        self.owner
            .as_deref()
            .or_else(|| self.auth.as_ref().map(|a| a.username.as_str()))
    }
}
