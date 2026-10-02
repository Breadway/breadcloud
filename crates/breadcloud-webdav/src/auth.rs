use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::state::{AppState, BasicAuthConfig};

/// HTTP Basic auth gate. When `state.auth` is `None` the server is open
/// (local development only). This is a placeholder: CLOUD-11 replaces it
/// with real multi-user accounts.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Real Nextcloud serves status.php unauthenticated: it is the first
    // request a client makes, before it has any credentials to send (the
    // "add account" flow uses it to confirm there is a Nextcloud-shaped
    // server at this URL at all). Gating it behind auth would break that
    // flow for every real client, mirall included.
    if req.method() == Method::GET && req.uri().path() == "/status.php" {
        return next.run(req).await;
    }

    let Some(auth) = &state.auth else {
        return next.run(req).await;
    };

    if check_basic_auth(req.headers(), auth) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

/// The actual HTTP Basic credential check (parse the `Authorization`
/// header, base64-decode, compare to `auth`). `pub` so `breadcloud-caldav`
/// can reuse the exact same check for its own auth gate rather than
/// keeping a second copy of the base64/header-parsing logic in sync by
/// hand -- see that crate's `auth` module for why the *middleware*
/// wrapper around this is not itself shared (different axum `State`
/// types per crate).
pub fn check_basic_auth(headers: &HeaderMap, auth: &BasicAuthConfig) -> bool {
    let Some(value) = headers.get("Authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(encoded) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, pass)) = text.split_once(':') else {
        return false;
    };
    user == auth.username && pass == auth.password
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        "WWW-Authenticate",
        "Basic realm=\"breadcloud\"".parse().unwrap(),
    );
    response
}
