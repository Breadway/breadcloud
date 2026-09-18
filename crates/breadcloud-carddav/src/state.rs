use std::sync::Arc;

use breadcloud_storage::AddressbookStore;
pub use breadcloud_webdav::BasicAuthConfig;

/// Shared state for the CardDAV router. Structurally identical to
/// `breadcloud_caldav::AppState` (an `Arc`-wrapped store plus an optional
/// shared `BasicAuthConfig`) -- see that crate's own doc comment on why
/// `auth` reuses `breadcloud_webdav::BasicAuthConfig` verbatim rather than
/// a third, identically-shaped type.
#[derive(Clone)]
pub struct AppState {
    pub addressbooks: Arc<AddressbookStore>,
    pub auth: Option<BasicAuthConfig>,
}

impl AppState {
    pub fn new(addressbooks: AddressbookStore) -> Self {
        AppState {
            addressbooks: Arc::new(addressbooks),
            auth: None,
        }
    }

    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Some(BasicAuthConfig {
            username: username.into(),
            password: password.into(),
        });
        self
    }
}
