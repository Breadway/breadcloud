use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// OPTIONS response, used by every WebDAV client to discover compliance
/// class and supported methods before doing anything else.
///
/// Advertises DAV class 1 only (no LOCK/UNLOCK): this server has no
/// locking, so claiming class 2 would tell clients to rely on a feature
/// that silently does nothing. Nextcloud's desktop client and iOS's
/// Files app both fall back to unconditional PUT/DELETE against a class
/// 1 server, which is exactly the core semantics this issue implements.
pub fn handle() -> Response {
    let mut response = StatusCode::OK.into_response();
    let headers = response.headers_mut();
    headers.insert("DAV", "1".parse().unwrap());
    headers.insert(
        "Allow",
        "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, MOVE, COPY"
            .parse()
            .unwrap(),
    );
    response
}
