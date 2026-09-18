//! Pre-addressbook discovery: `.well-known/carddav` and `OPTIONS`. Unlike
//! `breadcloud_caldav::handlers::discovery`, there is no
//! `current_user_principal`/`principal` handler here -- this crate does
//! not own the bare DAV root or the principal resource (see
//! `crate::path`'s doc comment); a DAVx5/iOS CardDAV setup walks the
//! *same* `current-user-principal` -> principal PROPFIND chain a CalDAV
//! setup does, just asking that second PROPFIND for
//! `card:addressbook-home-set` instead of (or alongside)
//! `cal:calendar-home-set`. See `breadcloud-server::combined` for how
//! that property gets onto `breadcloud-caldav`'s principal response.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// `GET /.well-known/carddav`, unauthenticated -- same exemption
/// `breadcloud_caldav::handlers::discovery::well_known` documents for
/// `.well-known/caldav` (the first request a client makes, before it has
/// necessarily sent credentials for the DAV root itself). Redirects to
/// the same DAV root CalDAV discovery starts from, since
/// `current-user-principal` discovery is shared between the two
/// protocols.
pub fn well_known() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("Location", "/remote.php/dav/")
        .body(Body::empty())
        .unwrap()
}

/// `OPTIONS` on any CardDAV resource: advertises the `addressbook` DAV
/// compliance class (RFC 6352 6.1) alongside class 1.
pub fn options() -> Response {
    let mut response = StatusCode::OK.into_response();
    let headers = response.headers_mut();
    headers.insert("DAV", "1, 3, addressbook".parse().unwrap());
    headers.insert(
        "Allow",
        "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCOL, REPORT"
            .parse()
            .unwrap(),
    );
    response
}
