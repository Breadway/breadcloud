//! A minimal, Nextcloud-versions-inspired DAV surface for browsing and
//! fetching a file's prior versions (CLOUD-4), mounted at
//! `/remote.php/dav/versions/<user>/<file-path...>`.
//!
//! Real Nextcloud addresses versions by a persistent numeric fileid
//! (`dav/versions/<user>/versions/<fileid>/<timestamp>`). This server has
//! no such id: `crate::fileid` derives a one-way hash from the resource's
//! *path*, not a reversible database key (see its doc comment), so there
//! is no `<fileid>` to route on here. Instead this surface is keyed by
//! the live file's own path, e.g. `PROPFIND
//! /remote.php/dav/versions/dev/docs/report.txt` lists that file's
//! retained versions, and `GET
//! /remote.php/dav/versions/dev/docs/report.txt/<version-id>` fetches
//! one.
//!
//! That is enough for a client (or a human with curl) that already knows
//! which file it wants history for to browse and restore prior versions.
//! It is not a drop-in implementation of Nextcloud's actual versions
//! protocol -- nothing here makes a stock Nextcloud desktop client
//! auto-discover or drive this surface the way it auto-discovers the
//! main files tree and the uploads staging area. See the workspace
//! README for that scope decision. There is deliberately no PUT/DELETE
//! here: restoring a version is "GET the old content, PUT it back to the
//! live path", which the existing PUT handler already snapshots as any
//! other overwrite -- adding a second, redundant "restore" verb here
//! would just be two ways to do the same write.
//!
//! The `<user>` path segment mirrors `handlers::uploads`'s layout. This
//! handler itself still does not cross-check it against the authenticated
//! identity (by the time a request reaches here, `state.storage`/
//! `state.versions` are already the authenticated account's own,
//! root-sandboxed instances -- see `breadcloud_webdav::AppState`), but
//! the real combined server does validate this segment before ever
//! dispatching here (CLOUD-11: `breadcloud-server::combined`'s
//! `url_scoped_user` check, covering this prefix alongside `files`,
//! `uploads`, `trashbin`, and every CalDAV/CardDAV per-user prefix),
//! rejecting a mismatch with 403 rather than letting it through.

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::DavPath;

use crate::error::{multi_status, storage_error_response};
use crate::state::AppState;
use crate::xml::content_type_for;

pub async fn dispatch(
    state: &AppState,
    method: &Method,
    _headers: &HeaderMap,
    remainder: &str,
) -> Response {
    let path = match DavPath::parse(remainder) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid versions path").into_response(),
    };

    let segments = path.segments();
    if segments.len() < 2 {
        return (
            StatusCode::NOT_FOUND,
            "expected /remote.php/dav/versions/<user>/<file-path...>",
        )
            .into_response();
    }
    // First segment is the user (see the module doc comment for why this
    // is not validated further); the rest names the live file.
    let user = &segments[0];
    let rest = &segments[1..];
    let prefix = format!("/remote.php/dav/versions/{user}");

    match method.as_str() {
        "PROPFIND" => match to_dav_path(rest) {
            Ok(file_path) => propfind_versions(state, &file_path, &prefix).await,
            Err(_) => (StatusCode::BAD_REQUEST, "invalid versions path").into_response(),
        },
        "GET" | "HEAD" => {
            let (file_segments, version_id) = rest.split_at(rest.len() - 1);
            match to_dav_path(file_segments) {
                Ok(file_path) => {
                    get_version(state, &file_path, &version_id[0], *method == Method::HEAD).await
                }
                Err(_) => (StatusCode::BAD_REQUEST, "invalid versions path").into_response(),
            }
        }
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            "only PROPFIND and GET/HEAD are supported on the versions surface",
        )
            .into_response(),
    }
}

fn to_dav_path(segments: &[String]) -> Result<DavPath, breadcloud_storage::path::InvalidPath> {
    segments
        .iter()
        .try_fold(DavPath::root(), |path, segment| path.child(segment))
}

/// Lists the retained versions of `file_path`, oldest first, as a plain
/// `DAV:`-only multistatus (no `oc:` properties: these entries are not
/// addressed by fileid and have no meaningful permissions/quota of their
/// own, so reusing the full files-tree renderer in `xml::response` would
/// claim more than this surface actually supports).
///
/// `prefix` (`/remote.php/dav/versions/<user>`) is prepended to every
/// href so it names a path the client can directly GET, the same as the
/// main files PROPFIND echoes back its own mount prefix (see
/// `xml::response::build_multistatus`).
async fn propfind_versions(state: &AppState, file_path: &DavPath, prefix: &str) -> Response {
    let versions = match state.versions.list(file_path).await {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    body.push_str("<d:multistatus xmlns:d=\"DAV:\">\n");
    for v in &versions {
        let href = format!("{prefix}{}/{}", file_path.to_request_path(), v.id);
        body.push_str("  <d:response>\n    <d:href>");
        body.push_str(&xml_escape(&href));
        body.push_str("</d:href>\n");
        body.push_str("    <d:propstat>\n      <d:prop>\n");
        body.push_str("        <d:resourcetype/>\n");
        body.push_str(&format!(
            "        <d:getcontentlength>{}</d:getcontentlength>\n",
            v.len
        ));
        body.push_str("        <d:getlastmodified>");
        body.push_str(&xml_escape(&httpdate::fmt_http_date(v.modified)));
        body.push_str("</d:getlastmodified>\n");
        body.push_str("        <d:getetag>");
        body.push_str(&xml_escape(&format!("\"{}\"", v.id)));
        body.push_str("</d:getetag>\n");
        body.push_str(
            "      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n",
        );
        body.push_str("  </d:response>\n");
    }
    body.push_str("</d:multistatus>\n");

    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// Fetches one specific retained version's content. Content-Type is
/// derived from the *live file's* name, not the numeric version id, so a
/// client gets a sensible type instead of `application/octet-stream` for
/// every version.
async fn get_version(
    state: &AppState,
    file_path: &DavPath,
    version_id: &str,
    head_only: bool,
) -> Response {
    match state.versions.read(file_path, version_id).await {
        Ok((file, len)) => {
            let builder = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Length", len.to_string())
                .header("Content-Type", content_type_for(file_path))
                .header("ETag", format!("\"{version_id}\""));
            if head_only {
                builder.body(Body::empty()).unwrap()
            } else {
                builder
                    .body(Body::from_stream(tokio_util::io::ReaderStream::new(file)))
                    .unwrap()
            }
        }
        Err(err) => storage_error_response(err),
    }
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
