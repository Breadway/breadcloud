//! File browser: list a directory, download/upload/delete a file, create
//! a folder. Deliberately reuses `breadcloud-webdav`'s own handlers for
//! every mutation (`handlers::delete::handle`, `handlers::mkcol::handle`,
//! `handlers::get::handle` for downloads) rather than re-implementing
//! DELETE-goes-to-trash, MKCOL's error mapping, or GET's etag/streaming
//! logic a second time -- this module's own code is only the HTML
//! rendering and the multipart/form plumbing a browser needs that the
//! DAV handlers do not.
//!
//! Trash/version browsing through the UI (CLOUD-4/CLOUD-5's existing DAV
//! surfaces) is deliberately not included here: the issue frames this as
//! "just a file browser," and a delete already routes through
//! `breadcloud-webdav`'s soft-delete (nothing is destructively lost, a
//! user can still recover a mistaken delete via a WebDAV client's
//! trashbin view or `breadcloud_webdav::handlers::trashbin` directly), so
//! adding a second browsing surface for it here would be scope beyond
//! what "minimal" asks for.

use axum::extract::{FromRequest, Multipart, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use breadcloud_storage::{DavPath, StorageError};
use breadcloud_webdav::AppState;

use crate::form::parse_urlencoded;
use crate::template::{
    breadcrumbs, encode_ui_path, html_escape, html_response, human_bytes, page, FILE_ICON,
    FOLDER_ICON,
};

/// Safety cap on a UI-uploaded file, matching the plain PUT path's own
/// buffered-body cap (`breadcloud_webdav::handlers::put::MAX_BODY_BYTES`,
/// not exported, so this is the same figure re-stated rather than
/// imported -- a browser file upload only ever exercises this path, never
/// the chunked-upload one real desktop sync clients use for anything
/// this large in practice).
///
/// `pub` (re-exported at the crate root) so `breadcloud-server::combined`
/// can size its `DefaultBodyLimit` layer to match this exactly, rather
/// than a second hand-copied constant: axum applies its own 2 MiB default
/// body-size limit to any request read through a `DefaultBodyLimit`-aware
/// extractor (`Multipart::from_request` below is the one such extractor
/// anywhere in this workspace, see `combined.rs`'s own layer for the
/// rest of that reasoning), silently overriding this cap unless the
/// combined router raises it to match.
pub const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024 * 1024;

pub async fn browse(state: &AppState, user: &str, path: &DavPath) -> Response {
    match state.storage.metadata(path).await {
        Ok(meta) if !meta.is_dir() => {
            return Redirect::to(&encode_ui_path("/ui/download", path)).into_response();
        }
        Ok(_) => {}
        Err(StorageError::NotFound) if path.is_root() => {}
        Err(err) => return breadcloud_webdav::error::storage_error_response(err),
    }

    let mut entries = match state.storage.list_dir(path).await {
        Ok(entries) => entries,
        Err(err) => return breadcloud_webdav::error::storage_error_response(err),
    };
    entries.sort_by(|a, b| {
        b.metadata.is_dir().cmp(&a.metadata.is_dir()).then_with(|| {
            a.path
                .name()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b.path.name().unwrap_or("").to_lowercase())
        })
    });

    let quota = state
        .storage
        .quota_report(state.quota_limit_bytes)
        .await
        .ok();

    let mut body = String::new();
    body.push_str("<h1>Files</h1>");
    if let Some(quota) = quota {
        let used = human_bytes(quota.used_bytes);
        body.push_str(&format!(r#"<div class="quota">{used} used"#));
        if let Some(limit) = state.quota_limit_bytes {
            let raw_pct = if limit == 0 {
                100.0
            } else {
                quota.used_bytes as f64 / limit as f64 * 100.0
            };
            let pct = raw_pct.min(100.0);
            // Same 80/90/100 percent-of-quota bands `breadcloud-events`'
            // `crossed_threshold` (CLOUD-13) already warns on, reused here
            // purely for the bar's color -- no event is emitted by
            // rendering a page, this is presentation only.
            let bar_class = if raw_pct >= 100.0 {
                " critical"
            } else if raw_pct >= 80.0 {
                " warn"
            } else {
                ""
            };
            body.push_str(&format!(
                " of {}<div class=\"quota-bar{bar_class}\"><div style=\"width:{pct:.1}%\"></div></div>",
                human_bytes(limit)
            ));
        }
        body.push_str("</div>");
    }
    body.push_str(&breadcrumbs(path));

    let dir_value = html_escape(&path.to_request_path());
    body.push_str(&format!(
        r#"<div class="toolbar">
<form method="post" action="/ui/upload" enctype="multipart/form-data">
  <input type="hidden" name="dir" value="{dir_value}">
  <input type="file" name="file" required>
  <button class="btn" type="submit">Upload</button>
</form>
<form method="post" action="/ui/mkdir">
  <input type="hidden" name="dir" value="{dir_value}">
  <input type="text" name="name" placeholder="New folder name" required>
  <button class="btn" type="submit">Create folder</button>
</form>
</div>"#
    ));

    if entries.is_empty() {
        body.push_str(r#"<p class="empty">This folder is empty.</p>"#);
    } else {
        body.push_str("<table><thead><tr><th>Name</th><th>Size</th><th>Modified</th><th></th></tr></thead><tbody>");
        for entry in &entries {
            let name = entry.path.name().unwrap_or("").to_string();
            let is_dir = entry.metadata.is_dir();
            let row_href = if is_dir {
                encode_ui_path("/ui/files", &entry.path)
            } else {
                encode_ui_path("/ui/download", &entry.path)
            };
            let label = if is_dir {
                format!("{name}/")
            } else {
                name.clone()
            };
            let size = if is_dir {
                "-".to_string()
            } else {
                human_bytes(entry.metadata.len)
            };
            let modified = crate::datetime::format_mtime(entry.metadata.modified);
            let full_path = html_escape(&entry.path.to_request_path());
            let escaped_label = html_escape(&label);
            // `row_href` is percent-encoded for the URL by
            // `encode_ui_path`, but percent-encoding and HTML-attribute-
            // escaping are different jobs: percent-encoding's own escape
            // set does not cover `&`, so a name containing an HTML
            // entity (e.g. `&quot;`) would otherwise survive into the
            // `href="..."` attribute untouched and let the browser
            // decode it back into a literal quote, breaking out of the
            // attribute. Both encodings are needed, one does not
            // substitute for the other.
            let row_href = html_escape(&row_href);
            let icon = if is_dir { FOLDER_ICON } else { FILE_ICON };
            let link_class = if is_dir {
                "entry-link is-dir"
            } else {
                "entry-link"
            };
            // The confirm() prompt deliberately does not interpolate the
            // file name into the inline JS: an attribute-escaped name is
            // still decoded back to raw text before the browser compiles
            // the onsubmit handler, so a name containing a quote could
            // otherwise break out of the JS string literal. A generic
            // prompt sidesteps that entirely rather than trying to get a
            // second, JS-flavored escaping right for one confirm dialog.
            body.push_str(&format!(
                r#"<tr><td><a href="{row_href}" class="{link_class}">{icon}<span class="entry-name">{escaped_label}</span></a></td><td class="size">{size}</td><td class="modified">{modified}</td><td class="actions">
<form method="post" action="/ui/delete" onsubmit="return confirm('Delete this item?');">
  <input type="hidden" name="path" value="{full_path}">
  <button class="btn danger" type="submit">Delete</button>
</form>
</td></tr>"#
            ));
        }
        body.push_str("</tbody></table>");
    }

    html_response(page("Files", user, "files", &body))
}

pub async fn download(state: &AppState, path: &DavPath) -> Response {
    breadcloud_webdav::handlers::get::handle(state, path, false).await
}

pub async fn upload(state: &AppState, req: Request) -> Response {
    let mut multipart = match Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    };

    let mut dir = String::from("/");
    let mut file_name: Option<String> = None;
    let mut data: Option<Vec<u8>> = None;

    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(err) => return (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
        };
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "dir" => {
                dir = field.text().await.unwrap_or_default();
            }
            "file" => {
                file_name = field.file_name().map(str::to_string);
                // Read chunk-by-chunk (rather than `field.bytes()`, which
                // buffers the whole field before this function ever gets
                // a size to check) so an oversized upload is rejected as
                // soon as it crosses the cap, not after it has already
                // been buffered in full.
                let mut buf: Vec<u8> = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if buf.len() + chunk.len() > MAX_UPLOAD_BYTES {
                                return (StatusCode::PAYLOAD_TOO_LARGE, "file too large")
                                    .into_response();
                            }
                            buf.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(err) => {
                            return (StatusCode::BAD_REQUEST, err.to_string()).into_response()
                        }
                    }
                }
                data = Some(buf);
            }
            _ => {}
        }
    }

    let Some(file_name) = file_name.filter(|n| !n.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "no file selected").into_response();
    };
    // A browser sends only the leaf name in `filename`, but treat it as
    // untrusted anyway and take the last path segment only, same
    // defense-in-depth `DavPath::parse` already applies to every other
    // path this server accepts from a client.
    let leaf = file_name.rsplit(['/', '\\']).next().unwrap_or(&file_name);
    let Ok(parent) = DavPath::parse(&dir) else {
        return (StatusCode::BAD_REQUEST, "invalid target folder").into_response();
    };
    let dest = parent.child(leaf);
    let data = data.unwrap_or_default();

    let _quota_guard = match state.quota_limit_bytes {
        Some(limit) => match state
            .storage
            .quota_gate(limit, &dest, data.len() as u64)
            .await
        {
            Ok(guard) => Some(guard),
            Err(err) => return breadcloud_webdav::error::storage_error_response(err),
        },
        None => None,
    };

    // Take a version only after the upload body is valid and the quota
    // gate accepted it, immediately before replacing the live file.
    let pending_snapshot =
        match breadcloud_webdav::versioning::snapshot_before_overwrite(state, &dest).await {
            Ok(snapshot) => snapshot,
            Err(err) => return breadcloud_webdav::error::storage_error_response(err),
        };

    match state.storage.write_bytes(&dest, &data).await {
        Ok(_) => {
            breadcloud_webdav::versioning::finish_snapshot(state, &dest, pending_snapshot).await;
            Redirect::to(&encode_ui_path("/ui/files", &parent)).into_response()
        }
        Err(err) => {
            breadcloud_webdav::versioning::discard_snapshot(state, &dest, pending_snapshot).await;
            breadcloud_webdav::error::storage_error_response(err)
        }
    }
}

pub async fn mkdir(state: &AppState, req: Request) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read request body").into_response(),
    };
    let form = parse_urlencoded(&body);
    let dir = form.get("dir").cloned().unwrap_or_default();
    let Some(name) = form.get("name").filter(|n| !n.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "missing folder name").into_response();
    };
    let Ok(parent) = DavPath::parse(&dir) else {
        return (StatusCode::BAD_REQUEST, "invalid target folder").into_response();
    };
    // Reject an embedded separator rather than letting it silently
    // address a nested path: this form field is meant to name one new
    // child of `parent`, not a multi-segment path.
    if name.contains('/') {
        return (StatusCode::BAD_REQUEST, "folder name cannot contain '/'").into_response();
    }
    let target = parent.child(name);

    let resp = breadcloud_webdav::handlers::mkcol::handle(state, &target).await;
    if resp.status().is_success() {
        Redirect::to(&encode_ui_path("/ui/files", &parent)).into_response()
    } else {
        resp
    }
}

pub async fn delete(state: &AppState, req: Request) -> Response {
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "could not read request body").into_response(),
    };
    let form = parse_urlencoded(&body);
    let Some(raw_path) = form.get("path") else {
        return (StatusCode::BAD_REQUEST, "missing path").into_response();
    };
    let Ok(target) = DavPath::parse(raw_path) else {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    };
    let parent = target.parent().unwrap_or_else(DavPath::root);

    let resp = breadcloud_webdav::handlers::delete::handle(state, &target).await;
    if resp.status().is_success() {
        Redirect::to(&encode_ui_path("/ui/files", &parent)).into_response()
    } else {
        resp
    }
}
