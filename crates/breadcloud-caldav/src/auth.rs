use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use breadcloud_webdav::auth::check_basic_auth;

use crate::state::{AppState, AuthenticatedUser};

/// HTTP Basic auth gate for the standalone CalDAV router (this crate's
/// own tests and any caller mounting it on its own). Structurally
/// identical to `breadcloud_webdav::auth::require_auth`: the actual
/// credential check is the literal same function
/// (`breadcloud_webdav::auth::check_basic_auth`), but the middleware
/// wrapper around it can't be shared as-is, since axum's
/// `middleware::from_fn_with_state` is generic over each crate's own
/// `State` type.
///
/// The real, combined `breadcloud-server` binary does not use this at
/// all: it applies one auth gate once, ahead of dispatching to either
/// protocol, using the same credentials -- see that binary's combined
/// router and the workspace README's "Sharing the HTTP server with
/// CalDAV" section.
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(auth) = &state.auth else {
        // Open mode: no fixed identity to stash, `dispatch` falls back to
        // `AppState.auth`'s own "anonymous" default (see
        // `crate::principal_href`).
        return next.run(req).await;
    };

    if check_basic_auth(req.headers(), auth) {
        // Single configured credential, so the authenticated username is
        // simply `auth.username` -- stashed so `dispatch` (via
        // `dispatch_handler`) sees the same per-request identity the real
        // combined server's shared gate provides, keeping this crate's
        // own standalone behavior (e.g. `current-user-principal`)
        // unchanged from before CLOUD-11.
        req.extensions_mut()
            .insert(AuthenticatedUser(auth.username.clone()));
        next.run(req).await
    } else {
        unauthorized()
    }
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        "WWW-Authenticate",
        "Basic realm=\"breadcloud\"".parse().unwrap(),
    );
    response
}
