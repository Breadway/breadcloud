use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use breadcloud_webdav::auth::check_basic_auth;

use crate::state::AppState;

/// HTTP Basic auth gate for the standalone CardDAV router (this crate's
/// own tests and any caller mounting it on its own). Identical in shape
/// to `breadcloud_caldav::auth::require_auth` and, like that one, unused
/// by the real combined `breadcloud-server` binary, which applies one
/// shared auth gate ahead of dispatching to whichever protocol a request
/// belongs to -- see `breadcloud-server`'s combined router.
pub async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(auth) = &state.auth else {
        return next.run(req).await;
    };

    if check_basic_auth(req.headers(), auth) {
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
