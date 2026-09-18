//! Pre-calendar discovery: `.well-known/caldav`, `current-user-principal`
//! at the bare DAV root, and the principal resource itself. This is what
//! DAVx5's "just enter the server address" CalDAV account type and iOS's
//! native Calendar account setup both walk before they ever PROPFIND a
//! calendar collection: `.well-known/caldav` (if the user entered a bare
//! domain) redirects to the real DAV root, a PROPFIND there for
//! `current-user-principal` returns the principal URL, and a PROPFIND on
//! *that* for `calendar-home-set` returns where the calendars actually
//! live (`crate::handlers::propfind::calendar_home`).

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::handlers::{read_propfind_mode, xml_response};
use crate::state::AppState;
use crate::xml::response;

/// `GET /.well-known/caldav`, unauthenticated (matching
/// `breadcloud_webdav::handlers::discovery::status`'s own
/// `status.php` exemption: this is the first request a client makes,
/// before it has necessarily entered credentials for the DAV root
/// itself). Redirects to the real DAV root, where discovery continues.
pub fn well_known() -> Response {
    Response::builder()
        .status(StatusCode::MOVED_PERMANENTLY)
        .header("Location", "/remote.php/dav/")
        .body(Body::empty())
        .unwrap()
}

/// `OPTIONS` on any CalDAV resource: advertises the `calendar-access`
/// DAV compliance class (RFC 4791 5.1) alongside class 1, and the
/// CalDAV-specific methods this server supports.
pub fn options() -> Response {
    let mut response = StatusCode::OK.into_response();
    let headers = response.headers_mut();
    headers.insert("DAV", "1, 3, calendar-access".parse().unwrap());
    headers.insert(
        "Allow",
        "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, MKCALENDAR, REPORT"
            .parse()
            .unwrap(),
    );
    response
}

/// PROPFIND on the bare DAV root (`/remote.php/dav` or
/// `/remote.php/dav/`), answering `current-user-principal`. This server
/// has one owner-equivalent user until CLOUD-11 adds real multi-user
/// accounts (the same placeholder-auth stance `breadcloud_webdav::auth`
/// takes), so "the current user" is simply the configured Basic-auth
/// username, not derived per-request from any session/token.
pub async fn current_user_principal(state: &AppState, req: Request) -> Response {
    let mode = read_propfind_mode(req).await;
    let principal_href = crate::principal_href(state);
    let xml =
        response::current_user_principal_multistatus("/remote.php/dav/", &principal_href, &mode);
    xml_response(xml)
}

/// PROPFIND on `/remote.php/dav/principals/<user>/`, answering
/// `calendar-home-set` (the property both DAVx5 and iOS need to find
/// where this user's calendars actually live) plus the smaller set of
/// principal-identity properties real clients also request alongside it.
///
/// `user` is taken from the URL as-is rather than cross-checked against
/// the authenticated identity: there is only one real account until
/// CLOUD-11, so there is nothing meaningful to mismatch yet (matching
/// `breadcloud_webdav`'s own single-user stance -- see that crate's
/// `handlers::propfind` permissions doc comment).
pub async fn principal(state: &AppState, user: &str, req: Request) -> Response {
    let _ = state; // no calendar lookup needed to answer a principal's own identity
    let mode = read_propfind_mode(req).await;
    let principal_href = format!("/remote.php/dav/principals/{user}/");
    let home_href = format!("/remote.php/dav/calendars/{user}/");
    let xml = response::principal_multistatus(&principal_href, &home_href, user, &mode);
    xml_response(xml)
}
