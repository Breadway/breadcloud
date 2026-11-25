//! Tiny `application/x-www-form-urlencoded` body parser.
//!
//! `breadcloud-webui`'s POST actions (`/ui/mkdir`, `/ui/delete`) are
//! plain HTML `<form>` posts with one or two fields each -- not worth a
//! `serde`+`axum::Form` dependency (nothing else in this workspace
//! depends on `serde` either, see docs/DESIGN.md's dependency
//! philosophy) when a form body this small is a couple of `split`s away
//! from a `HashMap`. Reuses the `percent-encoding` crate every other
//! `breadcloud-*` HTTP crate already depends on rather than adding a
//! second URL-decoding implementation.

use std::collections::HashMap;

use percent_encoding::percent_decode_str;

/// Parse a `key=value&key=value` body into a lookup map. Malformed pairs
/// (no `=`) are treated as a key with an empty value, matching how a
/// browser's own `application/x-www-form-urlencoded` encoder never
/// produces one but a hand-crafted request might.
pub fn parse_urlencoded(body: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(body);
    let mut out = HashMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(decode_field(key), decode_field(value));
    }
    out
}

/// `application/x-www-form-urlencoded` decodes `+` as a literal space
/// (distinct from plain percent-encoding, which leaves `+` alone) before
/// percent-decoding the rest.
fn decode_field(raw: &str) -> String {
    let spaced = raw.replace('+', " ");
    percent_decode_str(&spaced).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_pairs() {
        let form = parse_urlencoded(b"dir=%2Fdocs&name=New+Folder");
        assert_eq!(form.get("dir").map(String::as_str), Some("/docs"));
        assert_eq!(form.get("name").map(String::as_str), Some("New Folder"));
    }

    #[test]
    fn empty_body_is_empty_map() {
        assert!(parse_urlencoded(b"").is_empty());
    }

    #[test]
    fn key_with_no_value_is_empty_string() {
        let form = parse_urlencoded(b"path=");
        assert_eq!(form.get("path").map(String::as_str), Some(""));
    }
}
