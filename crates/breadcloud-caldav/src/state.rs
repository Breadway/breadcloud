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

/// The authenticated username for the current request, stashed into the
/// request's extensions by an auth gate (this crate's own [`crate::auth`]
/// for standalone use, or `breadcloud-server::combined`'s shared gate for
/// the real multi-user server) so a handler downstream of dispatch can
/// answer "who is asking" without re-deriving it from headers or trusting
/// a URL path segment (CLOUD-11). See `crate::principal_href_for` and
/// `handlers::discovery::current_user_principal`, the one place in this
/// crate that currently needs it.
#[derive(Clone)]
pub struct AuthenticatedUser(pub String);
