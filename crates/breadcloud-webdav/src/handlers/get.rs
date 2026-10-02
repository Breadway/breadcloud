use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use breadcloud_storage::{DavPath, Metadata, StorageError};
use tokio_util::io::ReaderStream;

use crate::error::storage_error_response;
use crate::state::AppState;
use crate::xml::content_type_for;

/// GET and HEAD share everything except whether the body is sent.
pub async fn handle(state: &AppState, path: &DavPath, head_only: bool) -> Response {
    match state.storage.open(path).await {
        Ok((file, metadata)) => {
            let etag = match state.storage.etag_for(path, &metadata).await {
                Ok(e) => e,
                Err(err) => return storage_error_response(err),
            };
            file_response(file, &metadata, &etag, path, head_only)
        }
        Err(StorageError::IsADirectory) => {
            if head_only {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/html; charset=utf-8")
                    .header("X-Content-Type-Options", "nosniff")
                    .header("Content-Security-Policy", "sandbox")
                    .body(Body::empty())
                    .unwrap()
            } else {
                directory_listing(state, path).await
            }
        }
        Err(err) => storage_error_response(err),
    }
}

fn file_response(
    file: tokio::fs::File,
    metadata: &Metadata,
    etag: &str,
    path: &DavPath,
    head_only: bool,
) -> Response {
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Length", metadata.len.to_string())
        .header("Content-Type", content_type_for(path))
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", "sandbox")
        .header("ETag", etag)
        .header("Last-Modified", httpdate::fmt_http_date(metadata.modified));

    let builder = if is_active_content(path) {
        builder.header("Content-Disposition", "attachment")
    } else {
        builder
    };

    if head_only {
        builder.body(Body::empty()).unwrap()
    } else {
        // Stream the file rather than buffering it: the target tree is a
        // few gigabytes and individual files (photos, videos) can be
        // large enough that reading them fully into memory per request
        // would be wasteful under concurrent downloads.
        builder
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap()
    }
}

/// WebDAV does not define GET on a collection in any useful way; real
/// sync/file-manager clients never issue it (they use PROPFIND). This is
/// a plain fallback so hitting a directory URL in a browser or with curl
/// gets something readable instead of an error.
async fn directory_listing(state: &AppState, path: &DavPath) -> Response {
    match state.storage.list_dir(path).await {
        Ok(entries) => {
            let mut html = String::from("<!doctype html>\n<html>\n<body>\n<ul>\n");
            if !path.is_root() {
                html.push_str("<li><a href=\"../\">..</a></li>\n");
            }
            for entry in entries {
                let name = entry.path.name().unwrap_or("").to_string();
                let is_dir = entry.metadata.is_dir();
                // A relative path prefix prevents names such as
                // `javascript:alert(1)` from being parsed as URL schemes.
                let href = format!("./{}", crate::xml::response::encode_segment(&name, is_dir));
                let label = if is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                };
                html.push_str(&format!(
                    "<li><a href=\"{href}\">{}</a></li>\n",
                    xml_escape_html(&label)
                ));
            }
            html.push_str("</ul>\n</body>\n</html>\n");
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("X-Content-Type-Options", "nosniff")
                .header("Content-Security-Policy", "sandbox")
                .body(Body::from(html))
                .unwrap()
        }
        Err(err) => storage_error_response(err),
    }
}

fn is_active_content(path: &DavPath) -> bool {
    matches!(
        content_type_for(path).as_str(),
        "text/html" | "application/xhtml+xml" | "image/svg+xml"
    )
}

fn xml_escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
