//! CardDAV (RFC 6352) HTTP layer for breadcloud (CLOUD-7).
//!
//! Turns a [`breadcloud_storage::AddressbookStore`] into an
//! [`axum::Router`] that speaks the CardDAV surface real clients (DAVx5
//! on Android, iOS's native Contacts account type) actually drive:
//! `PROPFIND` on addressbook collections and addressbook-home-set,
//! `MKCOL` (with an addressbook resourcetype -- CardDAV has no
//! `MKCALENDAR`-equivalent verb), `PUT`/`GET`/`DELETE` on individual
//! `.vcf` contact resources, and `REPORT` with `addressbook-query`,
//! `addressbook-multiget`, and `sync-collection` (RFC 6578).
//!
//! **Structural mirror of `breadcloud-caldav`, deliberately, not a fresh
//! design.** CardDAV (RFC 6352) and CalDAV (RFC 4791) are both WebDAV
//! extensions for exactly the same shape -- "a collection of typed
//! objects, each a small opaque text blob keyed by `UID`, with
//! sync-collection/ctag change tracking and a `PROPFIND`/`REPORT`-based
//! discovery and query surface" -- so this crate's storage pattern
//! (`AddressbookStore`, mirroring `CalendarStore`), discovery pattern
//! (home-set via the shared principal resource), and REPORT handling
//! (multiget / query-with-a-filter / sync-collection) are all directly
//! carried over from `breadcloud-caldav`'s CLOUD-6 implementation rather
//! than redesigned from first principles.
//!
//! **Shared abstraction, or duplication? Duplication, deliberately.**
//! Two real, working implementations of "a DAV extension for a
//! collection of typed text objects" now exist in this workspace
//! (`breadcloud-caldav` and this crate), which is exactly the point at
//! which "should these share a common generic layer" becomes a live
//! question, not a hypothetical one. This crate does not introduce one,
//! for the same reason `breadcloud-caldav`'s own README section declined
//! to build CalDAV on top of `breadcloud-webdav`'s file-oriented
//! `ResourceEntry`/`build_multistatus`: the two formats' *content*
//! layers are genuinely different in ways that would leak straight
//! through any shared abstraction taken further than "these are both
//! collections of `.txt`-shaped things" -- iCalendar's time-range
//! filtering, component types (`VEVENT`/`VTODO`), and `RRULE` handling
//! have no vCard analog, and vCard's `KIND:group`/`MEMBER` group model
//! and property text-match filtering have no iCalendar analog. A shared
//! trait or generic store built from only two data points (one calendar
//! shape, one contact shape) would either have to be so thin it saves
//! nothing beyond what's already reused concretely below, or would have
//! to grow speculative hooks for divergence neither format has hit yet
//! -- premature generalization from n=2, the same trap
//! `breadcloud_caldav::ical`'s own module doc comment calls out for
//! recurrence expansion. So this crate reuses concretely, the same way
//! `breadcloud-caldav` reuses from `breadcloud-webdav`:
//! - `breadcloud_webdav::auth::check_basic_auth` for the credential
//!   check (see `auth`);
//! - `breadcloud_webdav::xml::request::parse_propfind_body` for PROPFIND
//!   request parsing;
//! - `breadcloud_webdav::error::storage_error_response` for mapping a
//!   `StorageError` to an HTTP status;
//! - `breadcloud_storage::calendar::ChangeEntry`, directly, for
//!   `AddressbookStore::changes_since`'s sync diff (see that method's doc
//!   comment) -- this one is *not* a CalDAV-specific type despite living
//!   in the `calendar` module: it is just "a uid, and whether it was
//!   deleted", with nothing calendar-specific about its shape at all.
//!
//! Everything else -- path parsing, XML property sets, REPORT bodies,
//! and the storage layout itself -- is its own copy, structurally
//! parallel to `breadcloud-caldav`'s but independently owned, so a future
//! divergence in either format (recurrence expansion on one side, vCard
//! `PRODID`/`REV` handling on the other, say) never has to be threaded
//! back through a shared abstraction neither crate asked for.
//!
//! **Does not own the principal resource or the bare DAV root.** Unlike
//! `breadcloud-caldav`, this crate has no `Root`/`Principal` path variant
//! and no `current_user_principal`/`principal` handler: there is exactly
//! one principal per user regardless of how many DAV extensions it has
//! home-sets for, and `breadcloud-caldav` already owns
//! `/remote.php/dav/principals`. `breadcloud-server::combined` splices
//! `card:addressbook-home-set` onto `breadcloud-caldav`'s existing
//! principal PROPFIND response, the same additive-splice technique that
//! crate already uses to put `current-user-principal` onto a bare-root
//! WebDAV PROPFIND -- see that module and the workspace README's
//! "Addressbook discovery" section for why the principal-level splice,
//! not a second bare-root splice, is the right place: `calendar-home-set`
//! is discovered exactly that way today, and "discover the addressbook
//! the same way DAVx5/iOS already discover a calendar" means mirroring
//! that hop, not bypassing it.
//!
//! **One HTTP server, not three.** Same reasoning and same mechanism as
//! `breadcloud-caldav`'s own doc comment: `breadcloud-server::combined`
//! calls this crate's plain [`dispatch`] alongside
//! `breadcloud_webdav::dispatch` and `breadcloud_caldav::dispatch`, from
//! one shared top-level fallback under one shared auth layer, rather than
//! standing up a third HTTP server.

pub mod auth;
pub mod handlers;
pub mod path;
pub mod state;
pub mod vcard;
pub mod xml;

pub use state::AppState;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::Router;
use percent_encoding::percent_decode_str;

pub use path::CardPath;

/// The `addressbook-home-set` href for `user` -- exposed at the crate
/// root, not just used internally, so `breadcloud-server::combined` can
/// splice this onto `breadcloud-caldav`'s principal PROPFIND response
/// (see this crate's module doc comment and that crate's own
/// `principal_href`, which this deliberately mirrors: same user, same
/// principal, a different home-set property).
pub fn addressbook_home_href(user: &str) -> String {
    format!("/remote.php/dav/addressbooks/{user}/")
}

/// Whether `path` (the raw, still percent-encoded request path) belongs
/// to this crate. Checked against literal, non-overlapping prefixes
/// (`/remote.php/dav/addressbooks` and `/.well-known/carddav`) -- neither
/// the bare DAV root nor `/remote.php/dav/principals`, both of which stay
/// `breadcloud-caldav`'s (see this crate's module doc comment), and
/// neither collides with WebDAV's own prefixes (`files`, `uploads`,
/// `versions`, `trashbin`, `status.php`, `ocs/*`) or CalDAV's
/// (`principals`, `calendars`).
pub fn owns_path(path: &str) -> bool {
    path == "/.well-known/carddav" || path.starts_with("/remote.php/dav/addressbooks")
}

/// Build a standalone CardDAV router, with its own auth middleware layer.
/// Used by this crate's own tests, and available to any caller that
/// wants CardDAV without also wiring up WebDAV/CalDAV.
pub fn router(state: AppState) -> Router {
    Router::new()
        .fallback(dispatch_handler)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth::require_auth))
}

async fn dispatch_handler(State(state): State<AppState>, req: Request) -> Response {
    dispatch(state, req).await
}

/// The CardDAV request dispatcher, exposed directly (not just through
/// [`router`]) for the same reason `breadcloud_caldav::dispatch` is: a
/// combined server calls this from its own shared top-level fallback,
/// under one shared auth layer, rather than running a third HTTP server.
pub async fn dispatch(state: AppState, req: Request) -> Response {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();

    if raw_path == "/.well-known/carddav" {
        return handlers::discovery::well_known();
    }

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    if method.as_str() == "OPTIONS" {
        return handlers::discovery::options();
    }

    let Some(parsed) = CardPath::parse(&decoded) else {
        return (StatusCode::NOT_FOUND, "no such CardDAV resource").into_response();
    };
    let headers = req.headers().clone();

    match (&parsed, method.as_str()) {
        (CardPath::AddressbooksHome(user), "PROPFIND") => {
            handlers::propfind::addressbook_home(&state, user, req).await
        }
        (CardPath::Addressbook(user, book), "PROPFIND") => {
            handlers::propfind::addressbook_collection(&state, user, book, req).await
        }
        (CardPath::Addressbook(user, book), "MKCOL") => {
            handlers::mkcol::handle(&state, user, book, req).await
        }
        (CardPath::Addressbook(user, book), "REPORT") => {
            handlers::report::handle(&state, user, book, req).await
        }
        (CardPath::Contact(user, book, obj), "GET") => {
            handlers::contact::get(&state, user, book, obj).await
        }
        (CardPath::Contact(user, book, obj), "PUT") => {
            handlers::contact::put(&state, user, book, obj, req).await
        }
        (CardPath::Contact(user, book, obj), "DELETE") => {
            handlers::contact::delete(&state, user, book, obj, &headers).await
        }
        (CardPath::Contact(user, book, _), "PROPFIND") => {
            // Same "rare, but observed from a real client" allowance
            // breadcloud_caldav::dispatch makes for a Depth:0 PROPFIND
            // directly on an event resource.
            handlers::propfind::addressbook_collection(&state, user, book, req).await
        }
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method for this resource",
        )
            .into_response(),
    }
}
