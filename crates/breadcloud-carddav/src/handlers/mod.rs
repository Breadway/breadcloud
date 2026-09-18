pub mod contact;
pub mod discovery;
pub mod mkcol;
pub mod propfind;
pub mod report;

use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use breadcloud_webdav::error::multi_status;

use crate::xml::request::{parse_propfind_body, PropfindRequest};

/// Same fixed cap `breadcloud_caldav::handlers` uses: a PROPFIND/REPORT/
/// MKCOL body is a handful of small XML elements at most.
pub const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read a request body (bounded by [`MAX_REQUEST_BODY_BYTES`]), tolerating
/// a missing/empty body as empty.
pub async fn read_body(req: Request) -> Vec<u8> {
    match axum::body::to_bytes(req.into_body(), MAX_REQUEST_BODY_BYTES).await {
        Ok(b) => b.to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Read and parse a PROPFIND request body into which properties it asked
/// for.
pub async fn read_propfind_mode(req: Request) -> PropfindRequest {
    parse_propfind_body(&read_body(req).await)
}

/// Wrap a `multistatus` XML document into a 207 response with the right
/// content type.
pub fn xml_response(body: String) -> Response {
    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// PROPFIND `Depth` header -- identical parsing and identical "infinity
/// or missing collapses to One" stance as
/// `breadcloud_caldav::handlers::Depth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Zero,
    One,
}

pub fn parse_depth(headers: &HeaderMap) -> Depth {
    match headers.get("Depth").and_then(|v| v.to_str().ok()) {
        Some("0") => Depth::Zero,
        _ => Depth::One,
    }
}
