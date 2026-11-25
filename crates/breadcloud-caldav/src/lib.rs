//! CalDAV (RFC 4791) HTTP layer for breadcloud (CLOUD-6).
//!
//! Turns a [`breadcloud_storage::CalendarStore`] into an [`axum::Router`]
//! that speaks the subset of CalDAV real clients (DAVx5 on Android, iOS's
//! native Calendar account type) actually drive: principal and
//! calendar-home-set discovery, `PROPFIND` on calendar collections,
//! `MKCALENDAR`, `PUT`/`GET`/`DELETE` on individual `.ics` calendar
//! object resources, and `REPORT` with `calendar-query`,
//! `calendar-multiget`, and `sync-collection` (RFC 6578, plus the
//! `getctag` property clients poll between full syncs).
//!
//! **Relationship to `breadcloud-webdav`.** CalDAV is WebDAV-derived (RFC
//! 4791 extends RFC 4918: it is still `PROPFIND` and a `multistatus`
//! response format, still HTTP Basic auth, still the same kind of
//! collection/resource hierarchy), so this crate reuses what genuinely
//! transfers:
//! - `breadcloud_webdav::auth::check_basic_auth` for the actual
//!   credential check (see `auth`);
//! - `breadcloud_webdav::xml::request::parse_propfind_body` for parsing a
//!   PROPFIND body's requested-property list (namespace-agnostic local-name
//!   matching, exactly as applicable to a calendar property as a file one);
//! - `breadcloud_webdav::error::storage_error_response` for mapping a
//!   `breadcloud_storage::StorageError` to an HTTP status, since both
//!   crates' storage-layer errors mean the same thing (not found,
//!   conflict, and so on).
//!
//! What it does *not* reuse: `ResourceEntry`/`build_multistatus` (built
//! around a file's `Metadata`, not a calendar object -- see `xml::response`'s
//! doc comment), and there is no `REPORT` support in `breadcloud-webdav`
//! at all to share (WebDAV as implemented there never needed it; CalDAV
//! cannot do without it). Calendar objects themselves (iCalendar `.ics`
//! bytes) are stored and served as opaque content -- `crate::ical` reads
//! just enough of that content (a UID, a time range) to answer CalDAV
//! queries, it is not a general iCalendar library.
//!
//! **One HTTP server, not two.** This crate exposes both a standalone
//! [`router`] (used by its own test suite, and available to any caller
//! that only wants CalDAV) and a plain [`dispatch`] function + [`owns_path`]
//! path-ownership check, so `breadcloud-server` can run WebDAV and CalDAV
//! on one axum app, one port, and one auth gate: a single top-level
//! fallback asks `owns_path` which protocol a request belongs to and
//! calls the matching `dispatch`, under one shared auth middleware layer
//! built from the same credentials. See `breadcloud-server`'s combined
//! router and docs/DESIGN.md's "Sharing the HTTP server with
//! CalDAV" section.

pub mod auth;
pub mod handlers;
pub mod ical;
pub mod path;
pub mod state;
pub mod xml;

pub use state::{AppState, AuthenticatedUser};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::Router;
use percent_encoding::percent_decode_str;

pub use path::CalPath;

/// The `current-user-principal` href for this server's one
/// owner-equivalent user (see `handlers::discovery`'s doc comment on
/// `current_user_principal`). Exposed at the crate root, not just used
/// internally by that handler, so a combined server can also advertise
/// this property when answering a bare `/` WebDAV PROPFIND -- see
/// `breadcloud-server`'s `combined` module and docs/DESIGN.md's
/// "Bare-root discovery" note (CLOUD-6 change request: a real client that
/// only falls back to `.well-known/caldav` when the bare-root request
/// *errors*, not when it merely succeeds without this property, needs to
/// see it right there for "just enter a server address" discovery to
/// actually work).
pub fn principal_href(state: &AppState) -> String {
    let username = state
        .auth
        .as_ref()
        .map(|a| a.username.as_str())
        .unwrap_or("anonymous");
    principal_href_for(username)
}

/// Same as [`principal_href`], for a caller that already knows the
/// authenticated username directly rather than reading it off
/// `AppState.auth` (CLOUD-11: the real combined server resolves the
/// authenticated identity once, centrally, ahead of dispatch, so it has
/// this in hand without going through a fixed, per-`AppState` credential
/// -- see `breadcloud-server::combined`, which calls this instead of
/// [`principal_href`] for exactly that reason).
pub fn principal_href_for(user: &str) -> String {
    format!("/remote.php/dav/principals/{user}/")
}

/// Whether `path` (the raw, still percent-encoded request path) belongs
/// to this crate. Checked against literal, non-overlapping prefixes
/// (`/remote.php/dav/principals`, `/remote.php/dav/calendars`,
/// `/.well-known/caldav`, and the bare `/remote.php/dav[/]` root used for
/// `current-user-principal` discovery) so a combined server's top-level
/// dispatch can safely try this first and fall back to
/// `breadcloud_webdav::dispatch` for everything else -- none of these
/// prefixes collide with webdav's own (`files`, `uploads`, `versions`,
/// `trashbin`, `status.php`, `ocs/*`).
pub fn owns_path(path: &str) -> bool {
    path == "/.well-known/caldav"
        || path == "/remote.php/dav"
        || path == "/remote.php/dav/"
        || path.starts_with("/remote.php/dav/principals")
        || path.starts_with("/remote.php/dav/calendars")
}

/// Build a standalone CalDAV router, with its own auth middleware layer
/// (see `auth`). Used by this crate's own tests, and available to any
/// caller that wants CalDAV without also wiring up WebDAV.
pub fn router(state: AppState) -> Router {
    Router::new()
        .fallback(dispatch_handler)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth::require_auth))
}

async fn dispatch_handler(State(state): State<AppState>, req: Request) -> Response {
    // Standalone use (this crate's own tests, or any caller mounting
    // `router()` directly): `crate::auth::require_auth` is the only auth
    // gate in front of this handler, and it stashes the authenticated
    // username the same way the combined server's shared gate does (see
    // `crate::auth`) -- extract it here so `dispatch` behaves identically
    // either way.
    let user = req
        .extensions()
        .get::<AuthenticatedUser>()
        .map(|u| u.0.clone());
    dispatch(state, user.as_deref(), req).await
}

/// The CalDAV request dispatcher, exposed directly (not just through
/// [`router`]) for the same reason `breadcloud_webdav::dispatch` is: a
/// combined server calls this and `breadcloud_webdav::dispatch` from one
/// shared top-level fallback, under one shared auth layer, rather than
/// running two HTTP servers.
///
/// `authenticated_user`, when given, is who actually authenticated this
/// request (CLOUD-11) -- used only for `current-user-principal`
/// discovery at the bare DAV root (`handlers::discovery::current_user_principal`),
/// which otherwise has no per-request identity to answer with. Every
/// other handler here takes its `user` straight from the URL
/// (`CalPath`'s parsed segments): `breadcloud-server::combined` validates
/// that segment against `authenticated_user` itself, before ever calling
/// this function, so by the time a handler reads it off the URL it is
/// already known to match -- see that module's `url_scoped_user` check.
pub async fn dispatch(state: AppState, authenticated_user: Option<&str>, req: Request) -> Response {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();

    if raw_path == "/.well-known/caldav" {
        return handlers::discovery::well_known();
    }

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    let Some(parsed) = CalPath::parse(&decoded) else {
        return (StatusCode::NOT_FOUND, "no such CalDAV resource").into_response();
    };
    let headers = req.headers().clone();

    match (&parsed, method.as_str()) {
        (CalPath::Root, "PROPFIND") => {
            handlers::discovery::current_user_principal(&state, authenticated_user, req).await
        }
        (CalPath::Principal(user), "PROPFIND") => {
            handlers::discovery::principal(&state, user, req).await
        }
        (CalPath::CalendarsHome(user), "PROPFIND") => {
            handlers::propfind::calendar_home(&state, user, req).await
        }
        (CalPath::Calendar(user, cal), "PROPFIND") => {
            handlers::propfind::calendar_collection(&state, user, cal, req).await
        }
        (CalPath::Calendar(user, cal), "MKCALENDAR") => {
            handlers::mkcalendar::handle(&state, user, cal, req).await
        }
        (CalPath::Calendar(user, cal), "REPORT") => {
            handlers::report::handle(&state, user, cal, req).await
        }
        (CalPath::Event(user, cal, obj), "GET") => {
            handlers::event::get(&state, user, cal, obj).await
        }
        (CalPath::Event(user, cal, obj), "PUT") => {
            handlers::event::put(&state, user, cal, obj, req).await
        }
        (CalPath::Event(user, cal, obj), "DELETE") => {
            handlers::event::delete(&state, user, cal, obj, &headers).await
        }
        (CalPath::Event(user, cal, _), "PROPFIND") => {
            // A client PROPFINDing an individual event resource directly
            // (rare, but iOS has been observed doing a Depth:0 sanity
            // PROPFIND on a just-PUT resource) gets the same collection
            // machinery's answer for that one calendar, which already
            // reports `getetag` -- enough for the client's purposes.
            handlers::propfind::calendar_collection(&state, user, cal, req).await
        }
        (CalPath::Root, "OPTIONS")
        | (CalPath::Principal(_), "OPTIONS")
        | (CalPath::CalendarsHome(_), "OPTIONS")
        | (CalPath::Calendar(_, _), "OPTIONS")
        | (CalPath::Event(_, _, _), "OPTIONS") => handlers::discovery::options(),
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method for this resource",
        )
            .into_response(),
    }
}
