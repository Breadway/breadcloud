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
//! router and the workspace README's "Sharing the HTTP server with
//! CalDAV" section.

pub mod auth;
pub mod handlers;
pub mod ical;
pub mod path;
pub mod state;
pub mod xml;

pub use state::AppState;

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
/// `breadcloud-server`'s `combined` module and the workspace README's
/// "Bare-root discovery" note (CLOUD-6 change request: a real client that
/// only falls back to `.well-known/caldav` when the bare-root request
/// *errors*, not when it merely succeeds without this property, needs to
/// see it right there for "just enter a server address" discovery to
/// actually work).
pub fn principal_href(state: &AppState) -> String {
    let username = state
        .auth
        .as_ref()
        .map(|a| a.username.clone())
        .unwrap_or_else(|| "anonymous".to_string());
    format!("/remote.php/dav/principals/{username}/")
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
    dispatch(state, req).await
}

/// The CalDAV request dispatcher, exposed directly (not just through
/// [`router`]) for the same reason `breadcloud_webdav::dispatch` is: a
/// combined server calls this and `breadcloud_webdav::dispatch` from one
/// shared top-level fallback, under one shared auth layer, rather than
/// running two HTTP servers.
pub async fn dispatch(state: AppState, req: Request) -> Response {
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

    match (&parsed, method.as_str()) {
        (CalPath::Root, "PROPFIND") => {
            handlers::discovery::current_user_principal(&state, req).await
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
            handlers::event::delete(&state, user, cal, obj).await
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
