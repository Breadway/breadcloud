use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use breadcloud_storage::DavPath;

use crate::error::{multi_status, storage_error_response};
use crate::state::AppState;
use crate::xml::{build_multistatus, parse_propfind_body, ResourceEntry};

const MAX_PROPFIND_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
enum Depth {
    Zero,
    One,
    Infinity,
}

fn parse_depth(headers: &HeaderMap) -> Depth {
    match headers.get("Depth").and_then(|v| v.to_str().ok()) {
        Some("0") => Depth::Zero,
        Some("1") => Depth::One,
        Some("infinity") => Depth::Infinity,
        // RFC 4918 10.2: a missing Depth header on PROPFIND defaults to
        // infinity. In practice mirall and iOS always send one, so this
        // mostly matters for curl/manual testing.
        None => Depth::Infinity,
        Some(_) => Depth::One,
    }
}

pub async fn handle(
    state: &AppState,
    path: &DavPath,
    prefix: &str,
    headers: &HeaderMap,
    req: Request,
) -> Response {
    let depth = parse_depth(headers);

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_PROPFIND_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return axum::response::IntoResponse::into_response((
                axum::http::StatusCode::BAD_REQUEST,
                "could not read PROPFIND request body",
            ))
        }
    };
    let propfind_request = parse_propfind_body(&body_bytes);

    let self_metadata = match state.storage.metadata(path).await {
        Ok(m) => m,
        Err(err) => return storage_error_response(err),
    };

    // Per-user quota (CLOUD-11) when this account has one configured,
    // filesystem-level otherwise (see
    // breadcloud_storage::Storage::quota_report) -- only meaningful to
    // report on the collection being queried, not on every file inside
    // it.
    let quota = if self_metadata.is_dir() {
        state
            .storage
            .quota_report(state.quota_limit_bytes)
            .await
            .ok()
    } else {
        None
    };

    let self_etag = match state.storage.etag_for(path, &self_metadata).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };

    let mut entries = vec![ResourceEntry {
        path: path.clone(),
        metadata: self_metadata.clone(),
        etag: self_etag,
        quota,
    }];

    if self_metadata.is_dir() {
        let children = match depth {
            Depth::Zero => Ok(Vec::new()),
            Depth::One => state.storage.list_dir(path).await,
            Depth::Infinity => state.storage.list_recursive(path).await,
        };
        match children {
            Ok(children) => {
                for child in children {
                    let etag = match state.storage.etag_for(&child.path, &child.metadata).await {
                        Ok(e) => e,
                        Err(err) => return storage_error_response(err),
                    };
                    entries.push(ResourceEntry {
                        path: child.path,
                        metadata: child.metadata,
                        etag,
                        quota: None,
                    });
                }
            }
            Err(err) => return storage_error_response(err),
        }
    }

    let xml = build_multistatus(&entries, &propfind_request, prefix);
    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}
