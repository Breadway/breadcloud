//! Nextcloud Notes OCS API (CLOUD-8): a JSON REST view over markdown
//! files `breadcloud-webdav` already serves under each account's own
//! `Notes/` folder.
//!
//! **Why this exists at all**, per the issue's own investigation
//! (breadboard CLOUD-8): plain WebDAV file access already fully covers
//! creating, editing, versioning, trashing, and restoring a `.md` file
//! under `Notes/` -- confirmed live, nothing storage- or WebDAV-side is
//! notes-specific. What plain WebDAV does *not* cover is the small
//! companion REST API a purpose-built Notes client (confirmed in use
//! here: the official Nextcloud Notes mobile app) calls instead of a
//! WebDAV PUT/GET/PROPFIND cycle -- `/index.php/apps/notes/api/v1/...`,
//! JSON in and out, `id`-addressed rather than path-addressed, with its
//! own ETag-based optimistic concurrency distinct from (but reusing the
//! same underlying etags as) CLOUD-3's WebDAV preconditions. This crate
//! is that API, and nothing else: every actual read or write still goes
//! through the same `Storage`/`VersionStore` a WebDAV request against the
//! same path would, so a note is never a second, divergent representation
//! of anything -- see `note` and `handlers`'s own module docs for exactly
//! which primitives are reused where.
//!
//! **One HTTP server, not two**, the same `owns_path`/`dispatch` pattern
//! `breadcloud-caldav`, `breadcloud-carddav`, and `breadcloud-webui`
//! already use: `breadcloud-server::combined` routes anything this crate
//! claims here from its one shared fallback, under the one shared
//! CLOUD-11 auth gate every other protocol already goes through, against
//! that request's already-resolved per-account `breadcloud_webdav::AppState`
//! (its own `Notes/` tree, its own quota) -- there is no `<user>` segment
//! anywhere in this crate's URL space for a request to spoof, the same
//! reason `breadcloud-webui` needs no `url_scoped_user` check either.
//!
//! The endpoint contract itself (routes, JSON field names, ETag/`If-Match`
//! semantics, the `.attachments.<id>/` folder convention) is not guessed:
//! it is read directly off `nextcloud/notes`'s own source
//! (`appinfo/routes.php`, `lib/Controller/NotesApiController.php`,
//! `lib/Controller/Helper.php`, `lib/Service/Note.php`,
//! `lib/Service/NoteUtil.php`, `lib/AppInfo/Capabilities.php`) rather than
//! from documentation summaries alone -- see this crate's individual
//! modules for what was actually confirmed there and what was
//! deliberately left out as an extra a real client does not need to
//! function (chunked/paginated listing, sharing metadata, persisted
//! `favorite`/`readonly`).

pub mod handlers;
pub mod note;
pub mod path;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_webdav::AppState;
use percent_encoding::percent_decode_str;

pub use path::NotesRoute;

/// Whether `path` (the raw, still percent-encoded request path) belongs
/// to this crate. Mirrors `breadcloud_caldav::owns_path`'s role in the
/// combined server's top-level dispatch.
pub fn owns_path(path: &str) -> bool {
    path::owns_path(path)
}

/// The Notes API's request dispatcher. `webdav` is the authenticated
/// account's own already-resolved `AppState` (CLOUD-11) -- every note
/// this handles lives in, and only in, that account's own `Notes/` tree.
pub async fn dispatch(webdav: AppState, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let raw_path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    let Some(route) = NotesRoute::parse(&decoded) else {
        return (StatusCode::NOT_FOUND, "no such Notes API route").into_response();
    };

    match (route, method.as_str()) {
        (NotesRoute::Notes, "GET") => handlers::list(&webdav, &headers, &query).await,
        (NotesRoute::Notes, "POST") => match handlers::read_json_body(req).await {
            Ok(body) => handlers::create(&webdav, &body).await,
            Err(resp) => *resp,
        },
        (NotesRoute::Note(id), "GET") => handlers::get(&webdav, &id).await,
        (NotesRoute::Note(id), "PUT") => match handlers::read_json_body(req).await {
            Ok(body) => handlers::update(&webdav, &id, &headers, &body).await,
            Err(resp) => *resp,
        },
        (NotesRoute::Note(id), "DELETE") => handlers::delete(&webdav, &id).await,
        (NotesRoute::Settings, "GET") => handlers::settings_get().await,
        (NotesRoute::Settings, "PUT") => match handlers::read_json_body(req).await {
            Ok(body) => handlers::settings_put(&body).await,
            Err(resp) => *resp,
        },
        (NotesRoute::Attachment(id), "GET") => handlers::attachment_get(&webdav, &id, &query).await,
        (NotesRoute::Attachment(id), "POST") => {
            handlers::attachment_upload(&webdav, &id, req).await
        }
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "unsupported method for this route",
        )
            .into_response(),
    }
}
