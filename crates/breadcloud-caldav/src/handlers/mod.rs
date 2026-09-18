pub mod discovery;
pub mod event;
pub mod mkcalendar;
pub mod propfind;
pub mod report;

use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderMap;
use axum::response::Response;
use breadcloud_webdav::error::multi_status;

use crate::xml::request::{parse_propfind_body, PropfindRequest};

/// A PROPFIND/REPORT/MKCALENDAR body is a handful of small XML elements
/// at most; a generous fixed cap avoids a misbehaving or malicious client
/// making the server buffer an unbounded request body for what is never
/// legitimately more than a few kilobytes.
pub const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read a request body (bounded by [`MAX_REQUEST_BODY_BYTES`]), tolerating
/// a missing/empty body as empty rather than an error -- an empty
/// PROPFIND body is a valid `allprop` request (RFC 4918 14.1).
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
/// content type, shared by every handler that builds one (discovery,
/// PROPFIND, REPORT).
pub fn xml_response(body: String) -> Response {
    Response::builder()
        .status(multi_status())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// PROPFIND `Depth` header, parsed for the two values this server's
/// handlers actually distinguish. `infinity` (or a missing/unrecognized
/// header) collapses to `One`: no handler here ever needs to walk deeper
/// than "this collection's immediate children" (a calendar's events are
/// always addressed via REPORT, not a deep PROPFIND), so treating
/// `infinity` as "give me the children too" rather than rejecting it
/// outright is the more useful behavior for a client that sends it.
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
