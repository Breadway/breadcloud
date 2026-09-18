//! Minimal server-rendered web UI for breadcloud (CLOUD-12): a file
//! browser and a read-only calendar month view, covering the "open
//! Firefox and look at my files/calendar" access pattern the issue names
//! -- explicitly not a Nextcloud-style dashboard, activity feed, or
//! notification center.
//!
//! **One HTTP server, not two**, same pattern `breadcloud-caldav` and
//! `breadcloud-carddav` already use: this crate exposes a plain
//! [`dispatch`] function plus an [`owns_path`] path-ownership check, so
//! `breadcloud-server::combined` can route `/ui/*` requests here from its
//! one shared fallback, under the one shared auth gate every other
//! protocol already goes through, instead of standing up a fourth HTTP
//! server or a second, ad-hoc login flow.
//!
//! **Reuses the real authenticated identity, not a URL segment.**
//! `breadcloud-server::combined`'s `auth_gate` resolves who actually
//! authenticated this request (CLOUD-11) before [`dispatch`] is ever
//! called, and passes that identity in directly; every page and action
//! here operates against that account's own already-resolved
//! `breadcloud_webdav::AppState` (its own storage root, its own quota)
//! and the shared `breadcloud_caldav::AppState`'s calendars *for that
//! user*. There is no `<user>` segment anywhere in the `/ui/*` URL space
//! at all (see `path::UiRoute`) for a request to spoof in the first
//! place -- simpler than, and not a parallel implementation of,
//! CLOUD-11's `url_scoped_user` check the DAV surfaces need.
//!
//! Every mutation (delete, create folder) calls straight into
//! `breadcloud-webdav`'s own handlers (`handlers::delete::handle`,
//! `handlers::mkcol::handle`) rather than reimplementing them, and every
//! read reuses `breadcloud-storage`'s `Storage`/`CalendarStore` directly
//! -- see `files` and `calendar`'s own doc comments for what each page
//! does and does not reuse.

pub mod calendar;
pub mod datetime;
pub mod files;
pub mod form;
pub mod home;
pub mod path;
pub mod template;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_caldav::AppState as CaldavState;
use breadcloud_webdav::AppState as WebdavState;
use percent_encoding::percent_decode_str;

pub use files::MAX_UPLOAD_BYTES;
pub use path::UiRoute;

/// Whether `path` (the raw, still percent-encoded request path) belongs
/// to this crate. Mirrors `breadcloud_caldav::owns_path`/
/// `breadcloud_carddav::owns_path`'s role in the combined server's
/// top-level dispatch.
pub fn owns_path(path: &str) -> bool {
    path == "/ui" || path.starts_with("/ui/")
}

/// The web UI's request dispatcher. `webdav` is `user`'s own already-
/// resolved `AppState` (its own storage root and quota, CLOUD-11); `caldav`
/// is the shared `breadcloud-caldav` state, queried with `user` passed
/// explicitly rather than trusted from anywhere in the URL.
pub async fn dispatch(
    webdav: WebdavState,
    caldav: CaldavState,
    user: &str,
    req: Request,
) -> Response {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    let Some(route) = UiRoute::parse(&decoded) else {
        return (StatusCode::NOT_FOUND, "no such page").into_response();
    };

    // Matched on `method.as_str()`, the same convention
    // `breadcloud_webdav::dispatch`/`breadcloud_caldav::dispatch` already
    // use, rather than a `Method` pattern (an opaque wrapper type, not a
    // plain enum, so it is not eligible for pattern matching against its
    // `Method::GET`-style associated constants).
    match (route, method.as_str()) {
        (UiRoute::Home, "GET") => home::handle(user),
        (UiRoute::Files(path), "GET") => files::browse(&webdav, user, &path).await,
        (UiRoute::Download(path), "GET") => files::download(&webdav, &path).await,
        (UiRoute::Calendar, "GET") => calendar::month_view(&caldav, user, &query).await,
        (UiRoute::Upload, "POST") => files::upload(&webdav, req).await,
        (UiRoute::Mkdir, "POST") => files::mkdir(&webdav, req).await,
        (UiRoute::Delete, "POST") => files::delete(&webdav, req).await,
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method for this page",
        )
            .into_response(),
    }
}
