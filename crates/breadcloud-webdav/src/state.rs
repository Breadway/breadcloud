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
}
