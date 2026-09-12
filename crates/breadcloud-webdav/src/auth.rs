use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;

use crate::state::{AppState, BasicAuthConfig};

/// HTTP Basic auth gate. When `state.auth` is `None` the server is open
/// (local development only). This is a placeholder: CLOUD-11 replaces it
/// with real multi-user accounts.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(auth) = &state.auth else {
        return next.run(req).await;
    };

    if credentials_match(req.headers(), auth) {
        next.run(req).await
    } else {
        unauthorized()
    }
}

fn credentials_match(headers: &HeaderMap, auth: &BasicAuthConfig) -> bool {
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
