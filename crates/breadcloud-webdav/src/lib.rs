//! WebDAV (RFC 4918) HTTP layer for breadcloud.
//!
//! This crate turns a [`breadcloud_storage::Storage`] into an
//! [`axum::Router`] that speaks WebDAV: PROPFIND, GET/HEAD, PUT, DELETE,
//! MKCOL, MOVE, COPY, and OPTIONS, plus the extra surface a real
//! Nextcloud desktop client (mirall) needs to treat this server as a
//! sync backend: the `status.php`/OCS discovery endpoints it checks
//! before mounting the DAV tree at all (`handlers::discovery`), chunked
//! (NG chunking v2) upload (`handlers::uploads`), content-derived etags
//! with `If-Match`/`If-None-Match` conflict detection on PUT, and
//! `X-OC-Mtime` handling (`mtime`).
//!
//! It mounts at whatever path the caller nests it under (typically the
//! whole server, as in `breadcloud-server`). A bare mount at the server
//! root works too (for non-Nextcloud clients like rclone/davfs2, and
//! this crate's own tests), alongside the real
//! `/remote.php/dav/files/<user>/...` mount a Nextcloud client insists
//! on -- see [`split_mount_prefix`].
//!
//! Also covers per-file version history (CLOUD-4): every overwriting PUT
//! or chunked-upload finalize snapshots the content it is about to
//! replace (`versioning`), and a minimal DAV surface
//! (`handlers::versions`) lets a client browse and fetch prior versions.
//!
//! And soft-delete trash with a retention cap (CLOUD-5): DELETE moves a
//! resource (and its version history) into trash instead of removing it
//! (`trash::move_to_trash`), subject to an age and total-size cap enforced
//! after every trash/restore/purge (`breadcloud_storage::TrashStore`), and
//! a minimal DAV surface (`handlers::trashbin`) lets a client list,
//! restore, and early-purge trashed items.
//!
//! Scope note: this does not do WebDAV locking (LOCK/UNLOCK) -- a
//! separate, later issue in the breadcloud epic.

pub mod auth;
pub mod error;
pub mod fileid;
pub mod handlers;
pub mod mtime;
pub mod preconditions;
pub mod state;
pub mod trash;
pub mod versioning;
pub mod xml;

pub use state::{AppState, BasicAuthConfig};

use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
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

/// Real Nextcloud clients (mirall / nextcloudcmd) mount the file tree at
/// `/remote.php/dav/files/<user>/...`, not at the server root, and they
/// validate that every `<d:href>` in a PROPFIND response starts with the
/// exact prefix they requested -- confirmed against the real client:
/// without this, `OCC::LsColJob` logs "Invalid href ... expected starting
/// with ..." and fails the sync before a single file transfers. A bare
/// mount at the server root is also recognized, for non-Nextcloud
/// clients (rclone, davfs2) and this crate's own tests, which get an
/// empty prefix.
///
/// Splits a decoded request path into (mount prefix to echo back on
/// hrefs, storage-relative remainder).
pub(crate) fn split_mount_prefix(decoded: &str) -> (String, &str) {
    if let Some(rest) = decoded.strip_prefix("/remote.php/dav/files/") {
        let user_len = rest.find('/').unwrap_or(rest.len());
        let prefix = format!("/remote.php/dav/files/{}", &rest[..user_len]);
        let remainder = &rest[user_len..];
        return (prefix, if remainder.is_empty() { "/" } else { remainder });
    }
    if let Some(rest) = decoded.strip_prefix("/remote.php/webdav") {
        return (
            "/remote.php/webdav".to_string(),
            if rest.is_empty() { "/" } else { rest },
        );
    }
    (String::new(), decoded)
}

async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();
    let raw_path = req.uri().path().to_string();

    // Discovery endpoints a real Nextcloud client needs before it will
    // even attempt the DAV tree -- see `handlers::discovery`. Matched on
    // the raw (still percent-encoded, prefix-and-all) path since these
    // are not part of the DavPath-addressed tree at all.
    if method == Method::GET {
        if raw_path.ends_with("/status.php") {
            return handlers::discovery::status();
        }
        if raw_path.contains("/ocs/v1.php/cloud/capabilities")
            || raw_path.contains("/ocs/v2.php/cloud/capabilities")
        {
            return handlers::discovery::capabilities();
        }
        if raw_path.contains("/ocs/v1.php/cloud/user")
            || raw_path.contains("/ocs/v2.php/cloud/user")
        {
            let username = state
                .auth
                .as_ref()
                .map(|a| a.username.as_str())
                .unwrap_or("anonymous");
            return handlers::discovery::user(username);
        }
    }

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    // Chunked-upload staging lives in its own namespace and its own
    // Storage root (AppState::uploads), never inside the tree a
    // PROPFIND of the real files can see -- see `handlers::uploads`.
    if let Some(remainder) = decoded.strip_prefix("/remote.php/dav/uploads") {
        return handlers::uploads::dispatch(&state, &method, &headers, remainder, req).await;
    }

    // Version history (CLOUD-4) lives in its own namespace and its own
    // VersionStore root (AppState::versions), for the same reason as
    // uploads staging -- see `handlers::versions`.
    if let Some(remainder) = decoded.strip_prefix("/remote.php/dav/versions") {
        return handlers::versions::dispatch(&state, &method, &headers, remainder).await;
    }

    // Soft-delete trash (CLOUD-5) lives in its own namespace and its own
    // TrashStore root (AppState::trash), for the same reason as uploads
    // staging and version history -- see `handlers::trashbin`.
    if let Some(remainder) = decoded.strip_prefix("/remote.php/dav/trashbin") {
        return handlers::trashbin::dispatch(&state, &method, &headers, remainder).await;
    }

    let (prefix, dav_relative) = split_mount_prefix(&decoded);
    let path = match DavPath::parse(dav_relative) {
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
        "PROPFIND" => handlers::propfind::handle(&state, &path, &prefix, &headers, req).await,
        "MOVE" => handlers::copy_move::handle(&state, &path, &headers, false).await,
        "COPY" => handlers::copy_move::handle(&state, &path, &headers, true).await,
        _ => (StatusCode::METHOD_NOT_ALLOWED, "unsupported method").into_response(),
    }
}
