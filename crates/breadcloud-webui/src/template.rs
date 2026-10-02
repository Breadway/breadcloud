//! Shared page chrome (nav, breadcrumbs, icons) and small HTML-rendering
//! helpers used by both the file browser and the calendar view.
//!
//! Plain `format!`-built HTML strings, not a templating crate (askama or
//! similar): the whole UI is a handful of small pages, and pulling in a
//! templating engine's proc-macro/build-step machinery for that is more
//! than this genuinely-minimal issue calls for -- see the CLOUD-12 issue
//! text and the workspace README's dependency philosophy. Every value
//! interpolated from user- or file-supplied data goes through
//! [`html_escape`] first.
//!
//! **Styling (CLOUD-24).** One shared stylesheet, `style.css`, embedded at
//! compile time via `include_str!` and served as a plain static response
//! by [`style_response`] at `/ui/style.css` (see `lib.rs`'s route table) --
//! a real `<link rel="stylesheet">` per page rather than an inline
//! `<style>` block repeated in every response, while staying exactly as
//! dependency-free as the rest of this crate (no static-file-serving
//! crate, no build step: the CSS is just another `&'static str` baked into
//! the binary, the same way `include_str!` would embed any other asset).

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use breadcloud_storage::DavPath;

/// The shared stylesheet's source, served verbatim by [`style_response`].
pub const STYLE_CSS: &str = include_str!("style.css");

/// Wrap a rendered HTML string in a `200 OK` response with the right
/// content type, so every handler that renders a page does not repeat
/// this `Response::builder()` chain.
pub fn html_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

/// `GET /ui/style.css`: the shared stylesheet every page links to. A
/// short, cache-friendly max-age (not "forever" -- there is no
/// content-hashed filename here to bust a stale cache after a redeploy,
/// so an hour keeps a browser from re-fetching it on every single page
/// load without also risking someone staring at last week's CSS for very
/// long after this binary is updated).
pub fn style_response() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/css; charset=utf-8")
        .header("Cache-Control", "public, max-age=3600")
        .body(Body::from(STYLE_CSS))
        .unwrap()
}

/// Inline SVG icons for the file listing (`files.rs`), distinguishing a
/// folder row from a plain file row without pulling in an icon font or
/// image assets -- just two small `<path>`s, colored via `currentColor` so
/// `.entry-link.is-dir .entry-icon`'s CSS rule can recolor the folder icon
/// without a second copy of it.
pub const FOLDER_ICON: &str = r#"<svg class="entry-icon" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M1.5 3.5C1.5 2.94772 1.94772 2.5 2.5 2.5H6L7.5 4H13.5C14.0523 4 14.5 4.44772 14.5 5V12.5C14.5 13.0523 14.0523 13.5 13.5 13.5H2.5C1.94772 13.5 1.5 13.0523 1.5 12.5V3.5Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>"#;
pub const FILE_ICON: &str = r#"<svg class="entry-icon" viewBox="0 0 16 16" fill="none" xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M4 1.5H9.5L12.5 4.5V14C12.5 14.2761 12.2761 14.5 12 14.5H4C3.72386 14.5 3.5 14.2761 3.5 14V2C3.5 1.72386 3.72386 1.5 4 1.5Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/><path d="M9.5 1.5V4C9.5 4.27614 9.72386 4.5 10 4.5H12.5" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>"#;

pub fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Human-readable byte size (`1.5 MB`, `320 KB`, `42 B`), binary units to
/// match every other size this codebase already reports in (quota,
/// version/trash retention caps).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Percent-encode a `DavPath` for use in a `/ui/...` link, mirroring
/// `breadcloud_webdav::xml::response::encode_href`'s own per-segment
/// encoding (that function is `pub(crate)` to its own crate, so this is a
/// second small copy rather than a cross-crate dependency for one
/// helper).
pub fn encode_ui_path(prefix: &str, path: &DavPath) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    const ENCODE_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?');

    let mut out = String::from(prefix);
    for seg in path.segments() {
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(&utf8_percent_encode(seg, ENCODE_SET).to_string());
    }
    if path.is_root() && !out.ends_with('/') {
        // No segments at all (the storage root): keep the trailing slash
        // so relative browser links on that page resolve correctly. A
        // path with real segments never gets one added here -- a file's
        // link must stay a file's link, not silently turn into what
        // looks like a directory's.
        out.push('/');
    }
    out
}

pub fn breadcrumbs(path: &DavPath) -> String {
    let mut out = String::from(r#"<nav class="breadcrumbs" aria-label="Breadcrumb">"#);
    out.push_str(r#"<a href="/ui/files/">files</a>"#);
    let mut built = DavPath::root();
    for seg in path.segments() {
        // These segments came from an already validated `DavPath`.
        built = built
            .child(seg)
            .expect("DavPath segments are safe child names");
        out.push_str(r#"<span class="crumb-sep">/</span>"#);
        // Same two-step encoding `files.rs`'s row links need: percent-
        // encode for the URL, then HTML-attribute-escape the result
        // before interpolating it into `href="..."` -- percent-encoding
        // alone leaves `&` untouched, so an unescaped `&quot;` etc in a
        // directory name would otherwise decode back to a literal quote
        // in the browser and break out of the attribute.
        out.push_str(&format!(
            r#"<a href="{}">{}</a>"#,
            html_escape(&encode_ui_path("/ui/files", &built)),
            html_escape(seg)
        ));
    }
    out.push_str("</nav>");
    out
}

/// Wrap `body` in the shared page chrome. `active` is `"files"` or
/// `"calendar"`, used only to underline the matching nav link.
pub fn page(title: &str, user: &str, active: &str, body: &str) -> String {
    let title = html_escape(title);
    let user = html_escape(user);
    let files_class = if active == "files" {
        " class=\"active\""
    } else {
        ""
    };
    let cal_class = if active == "calendar" {
        " class=\"active\""
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - breadcloud</title>
<link rel="stylesheet" href="/ui/style.css">
</head>
<body>
<header class="topbar">
  <div class="brand">breadcloud</div>
  <nav>
    <a href="/ui/files/"{files_class}>Files</a>
    <a href="/ui/calendar"{cal_class}>Calendar</a>
  </nav>
  <div class="user">{user}</div>
</header>
<main>
{body}
</main>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_all_five_html_special_chars() {
        assert_eq!(
            html_escape("<a href=\"x\">&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn human_bytes_picks_the_right_unit() {
        assert_eq!(human_bytes(42), "42 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn encode_ui_path_percent_encodes_segments() {
        let path = DavPath::parse("/My Docs/a#b.txt").unwrap();
        assert_eq!(
            encode_ui_path("/ui/files", &path),
            "/ui/files/My%20Docs/a%23b.txt"
        );
    }

    #[test]
    fn encode_ui_path_root_keeps_trailing_slash() {
        assert_eq!(encode_ui_path("/ui/files", &DavPath::root()), "/ui/files/");
    }

    #[test]
    fn style_response_serves_the_embedded_stylesheet_as_css() {
        let response = style_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "text/css; charset=utf-8"
        );
    }

    #[test]
    fn page_links_the_shared_stylesheet_and_does_not_inline_one() {
        let html = page("Files", "alice", "files", "<p>hi</p>");
        assert!(html.contains(r#"<link rel="stylesheet" href="/ui/style.css">"#));
        assert!(!html.contains("<style>"));
    }

    #[test]
    fn breadcrumbs_separates_segments_with_a_styled_span_not_raw_text() {
        let path = DavPath::parse("/Docs/2026").unwrap();
        let html = breadcrumbs(&path);
        assert!(html.contains(r#"<span class="crumb-sep">/</span>"#));
        // The old plain-text " / " separator is gone -- every segment
        // boundary now goes through the styled span above instead.
        assert!(!html.contains(" / "));
    }
}
