use std::sync::Arc;

use breadcloud_storage::CalendarStore;
pub use breadcloud_webdav::BasicAuthConfig;

/// Shared state for the CalDAV router.
///
/// `auth` reuses `breadcloud_webdav::BasicAuthConfig` verbatim rather than
/// a second, identically-shaped type: this crate's own [`crate::auth`]
/// checks credentials the same way webdav's does (via
/// `breadcloud_webdav::auth::check_basic_auth`), and the combined server
/// binary configures both crates' `AppState.auth` from the same
/// `BREADCLOUD_USER`/`BREADCLOUD_PASS` pair, so sharing the type makes
/// "these are the same credentials" a fact the compiler can check instead
/// of just a convention two independently-typed configs have to be kept
/// in sync by hand.
#[derive(Clone)]
pub struct AppState {
    pub calendars: Arc<CalendarStore>,
    pub auth: Option<BasicAuthConfig>,
}

impl AppState {
    pub fn new(calendars: CalendarStore) -> Self {
        AppState {
            calendars: Arc::new(calendars),
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
