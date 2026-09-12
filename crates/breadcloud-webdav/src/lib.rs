//! WebDAV (RFC 4918) HTTP layer for breadcloud.
//!
//! This crate turns a [`breadcloud_storage::Storage`] into an
//! [`axum::Router`] that speaks WebDAV: PROPFIND, GET/HEAD, PUT, DELETE,
//! MKCOL, MOVE, COPY, and OPTIONS. It mounts at whatever path the caller
//! nests it under (typically the whole server, as in `breadcloud-server`,
//! since a plain file-sync client expects WebDAV at the server root).
//!
//! Scope note: this covers core RFC 4918 semantics only. It does not yet
//! do WebDAV locking (LOCK/UNLOCK) or streamed/chunked uploads -- both are
//! separate, later issues in the breadcloud epic.

pub mod auth;
pub mod error;
pub mod handlers;
pub mod state;
pub mod xml;

pub use state::{AppState, BasicAuthConfig};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::Router;
use breadcloud_storage::DavPath;
use percent_encoding::percent_decode_str;

/// Build the WebDAV router. Every HTTP method is handled through a
/// single fallback rather than axum's per-method routing, because
/// PROPFIND/MKCOL/MOVE/COPY are not among the small fixed set axum's
/// `MethodFilter` recognizes.
pub fn router(state: AppState) -> Router {
    Router::new()
        .fallback(dispatch)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth::require_auth))
}

async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let raw_path = req.uri().path().to_string();

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };
    let path = match DavPath::parse(&decoded) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid path").into_response(),
    };

    match method.as_str() {
        "OPTIONS" => handlers::options::handle(),
        "GET" => handlers::get::handle(&state, &path, false).await,
        "HEAD" => handlers::get::handle(&state, &path, true).await,
        "PUT" => handlers::put::handle(&state, &path, req).await,
        "DELETE" => handlers::delete::handle(&state, &path).await,
        "MKCOL" => handlers::mkcol::handle(&state, &path).await,
        "PROPFIND" => handlers::propfind::handle(&state, &path, &headers, req).await,
        "MOVE" => handlers::copy_move::handle(&state, &path, &headers, false).await,
        "COPY" => handlers::copy_move::handle(&state, &path, &headers, true).await,
        _ => (StatusCode::METHOD_NOT_ALLOWED, "unsupported method").into_response(),
    }
}
