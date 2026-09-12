use std::sync::Arc;

use breadcloud_storage::Storage;

/// Shared state for the whole WebDAV router.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<Storage>,
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
    pub fn new(storage: Storage) -> Self {
        AppState {
            storage: Arc::new(storage),
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
