//! Shared page chrome (nav, minimal styling) and small HTML-rendering
//! helpers used by both the file browser and the calendar view.
//!
//! Plain `format!`-built HTML strings, not a templating crate (askama or
//! similar): the whole UI is a handful of small pages, and pulling in a
//! templating engine's proc-macro/build-step machinery for that is more
//! than this genuinely-minimal issue calls for -- see the CLOUD-12 issue
//! text and the workspace README's dependency philosophy. Every value
//! interpolated from user- or file-supplied data goes through
//! [`html_escape`] first.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use breadcloud_storage::DavPath;

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

const CSS: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  color: #1b1b1b;
  background: #fafafa;
}
@media (prefers-color-scheme: dark) {
  body { color: #e8e8e8; background: #17181a; }
  a { color: #7fb2ff; }
  table, .card { background: #212327; border-color: #33353a; }
  th { background: #26282c; }
  tr:hover { background: #26282c; }
  .btn { background: #2a2c30; border-color: #3a3c41; color: #e8e8e8; }
  .btn.danger { color: #ff9b9b; }
  header { background: #1e1f22; border-color: #33353a; }
  input[type=text], input[type=file] { background: #1b1c1f; color: #e8e8e8; border-color: #3a3c41; }
  .quota-bar { background: #33353a; }
}
a { color: #1d6fd6; text-decoration: none; }
a:hover { text-decoration: underline; }
header {
  display: flex;
  align-items: center;
  gap: 1.5rem;
  padding: 0.75rem 1.25rem;
  background: #ffffff;
  border-bottom: 1px solid #e2e2e2;
}
header .brand { font-weight: 600; }
header nav { display: flex; gap: 1rem; flex: 1; }
header nav a.active { font-weight: 600; text-decoration: underline; }
header .user { color: #777; font-size: 0.9rem; }
main { max-width: 960px; margin: 0 auto; padding: 1.25rem; }
h1 { font-size: 1.25rem; }
.breadcrumbs { margin-bottom: 1rem; font-size: 0.95rem; }
.breadcrumbs a { margin-right: 0.15rem; }
table { width: 100%; border-collapse: collapse; background: #fff; border: 1px solid #e2e2e2; }
th, td { text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid #eee; }
th { background: #f4f4f4; font-weight: 600; font-size: 0.85rem; }
tr:hover { background: #f7f9fc; }
td.size, td.modified { white-space: nowrap; color: #666; font-size: 0.9rem; }
.actions form { display: inline; }
.btn {
  display: inline-block;
  padding: 0.3rem 0.7rem;
  border: 1px solid #ccc;
  border-radius: 4px;
  background: #f4f4f4;
  color: inherit;
  cursor: pointer;
  font-size: 0.85rem;
}
.btn.danger { color: #b3261e; }
.toolbar { display: flex; gap: 1rem; flex-wrap: wrap; align-items: center; margin: 1rem 0; }
.toolbar form { display: flex; gap: 0.4rem; align-items: center; }
input[type=text] { padding: 0.3rem 0.5rem; border: 1px solid #ccc; border-radius: 4px; }
.quota { font-size: 0.85rem; color: #666; margin-bottom: 0.75rem; }
.quota-bar { height: 6px; border-radius: 3px; background: #e2e2e2; overflow: hidden; margin-top: 0.2rem; }
.quota-bar > div { height: 100%; background: #1d6fd6; }
.month-nav { display: flex; align-items: center; gap: 1rem; margin-bottom: 1rem; }
.month-nav h1 { margin: 0; flex: 1; text-align: center; }
table.month { table-layout: fixed; }
table.month th { text-align: center; }
table.month td { vertical-align: top; height: 6rem; padding: 0.3rem; font-size: 0.8rem; }
table.month td.outside { color: #bbb; }
@media (prefers-color-scheme: dark) { table.month td.outside { color: #555; } }
table.month .daynum { font-weight: 600; margin-bottom: 0.2rem; }
.event { display: block; border-radius: 3px; padding: 0.05rem 0.3rem; margin-bottom: 0.15rem; background: #dbe9ff; color: #0b2e63; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cal-list { list-style: none; padding: 0; margin: 0 0 1rem 0; display: flex; gap: 0.75rem; flex-wrap: wrap; }
.cal-list li { padding: 0.15rem 0.5rem; border-radius: 4px; background: #eef2f7; font-size: 0.85rem; }
.empty { color: #888; font-style: italic; padding: 1rem 0; }
"#;

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
    let mut out = String::from(r#"<div class="breadcrumbs">"#);
    out.push_str(r#"<a href="/ui/files/">files</a>"#);
    let mut built = DavPath::root();
    for seg in path.segments() {
        built = built.child(seg);
        out.push_str(" / ");
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
    out.push_str("</div>");
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
<style>{CSS}</style>
</head>
<body>
<header>
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
}
