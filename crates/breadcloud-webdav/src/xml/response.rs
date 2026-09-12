use breadcloud_storage::{DavPath, Metadata, Quota};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use super::request::PropfindRequest;

/// Characters percent-encoded in an href path segment, beyond what
/// `CONTROLS` already covers. Deliberately narrower than a full
/// `NON_ALPHANUMERIC` set so ordinary filenames stay readable.
const PATH_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'^')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'\\')
    .add(b'[')
    .add(b']');

/// One resource to render as a `<d:response>` block.
///
/// Properties outside [`applicable_props`]'s set (Nextcloud's `oc:`/`nc:`
/// namespaced extension properties, WebDAV locking properties, and so on)
/// correctly come back 404 inside their own propstat when requested, per
/// RFC 4918 9.1 -- that is not a bug, it is how clients probe for
/// optional support.
pub struct ResourceEntry {
    pub path: DavPath,
    pub metadata: Metadata,
    /// `Some` only for the collections we want to report quota on
    /// (currently: any directory, computed from the filesystem backing
    /// the storage root -- see [`breadcloud_storage::Storage::quota`]).
    pub quota: Option<Quota>,
}

/// Build a full `multistatus` XML document for a PROPFIND response.
pub fn build_multistatus(resources: &[ResourceEntry], request: &PropfindRequest) -> String {
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    body.push_str("<d:multistatus xmlns:d=\"DAV:\">\n");

    for entry in resources {
        body.push_str("  <d:response>\n");
        body.push_str("    <d:href>");
        body.push_str(&xml_escape(&encode_href(
            &entry.path,
            entry.metadata.is_dir(),
        )));
        body.push_str("</d:href>\n");

        match request {
            PropfindRequest::PropName => {
                let applicable = applicable_props(entry);
                body.push_str("    <d:propstat>\n      <d:prop>\n");
                for name in applicable {
                    body.push_str(&format!("        <d:{name}/>\n"));
                }
                body.push_str("      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n");
            }
            PropfindRequest::AllProp => {
                let applicable = applicable_props(entry);
                body.push_str("    <d:propstat>\n      <d:prop>\n");
                body.push_str(&render_prop_values(entry, &applicable));
                body.push_str("      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n");
            }
            PropfindRequest::Prop(requested) => {
                let applicable = applicable_props(entry);
                let is_applicable = |n: &str| applicable.contains(&n);

                let found: Vec<&str> = requested
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|n| is_applicable(n))
                    .collect();
                let missing: Vec<&str> = requested
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|n| !is_applicable(n))
                    .collect();

                if !found.is_empty() {
                    body.push_str("    <d:propstat>\n      <d:prop>\n");
                    body.push_str(&render_prop_values(entry, &found));
                    body.push_str("      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n");
                }
                if !missing.is_empty() {
                    body.push_str("    <d:propstat>\n      <d:prop>\n");
                    for name in missing {
                        body.push_str(&format!("        <d:{name}/>\n"));
                    }
                    body.push_str(
                        "      </d:prop>\n      <d:status>HTTP/1.1 404 Not Found</d:status>\n    </d:propstat>\n",
                    );
                }
            }
        }

        body.push_str("  </d:response>\n");
    }

    body.push_str("</d:multistatus>\n");
    body
}

/// Properties we can actually answer for this specific resource (e.g.
/// `getcontentlength` only makes sense for a file, quota only for a
/// collection whose quota we computed).
fn applicable_props(entry: &ResourceEntry) -> Vec<&'static str> {
    let mut props = vec!["resourcetype", "getetag", "getlastmodified", "displayname"];
    if !entry.metadata.is_dir() {
        props.push("getcontentlength");
        props.push("getcontenttype");
    }
    if entry.metadata.is_dir() && entry.quota.is_some() {
        props.push("quota-used-bytes");
        props.push("quota-available-bytes");
    }
    props
}

fn render_prop_values(entry: &ResourceEntry, names: &[&str]) -> String {
    let mut out = String::new();
    for name in names {
        match *name {
            "resourcetype" => {
                if entry.metadata.is_dir() {
                    out.push_str("        <d:resourcetype><d:collection/></d:resourcetype>\n");
                } else {
                    out.push_str("        <d:resourcetype/>\n");
                }
            }
            "getetag" => {
                out.push_str("        <d:getetag>");
                out.push_str(&xml_escape(&entry.metadata.etag()));
                out.push_str("</d:getetag>\n");
            }
            "getlastmodified" => {
                out.push_str("        <d:getlastmodified>");
                out.push_str(&xml_escape(&httpdate::fmt_http_date(
                    entry.metadata.modified,
                )));
                out.push_str("</d:getlastmodified>\n");
            }
            "getcontentlength" => {
                out.push_str(&format!(
                    "        <d:getcontentlength>{}</d:getcontentlength>\n",
                    entry.metadata.len
                ));
            }
            "getcontenttype" => {
                out.push_str("        <d:getcontenttype>");
                out.push_str(&xml_escape(&content_type_for(&entry.path)));
                out.push_str("</d:getcontenttype>\n");
            }
            "displayname" => {
                out.push_str("        <d:displayname>");
                out.push_str(&xml_escape(entry.path.name().unwrap_or("")));
                out.push_str("</d:displayname>\n");
            }
            "quota-used-bytes" => {
                if let Some(q) = &entry.quota {
                    out.push_str(&format!(
                        "        <d:quota-used-bytes>{}</d:quota-used-bytes>\n",
                        q.used_bytes
                    ));
                }
            }
            "quota-available-bytes" => {
                if let Some(q) = &entry.quota {
                    out.push_str(&format!(
                        "        <d:quota-available-bytes>{}</d:quota-available-bytes>\n",
                        q.available_bytes
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

/// Best-effort content type from the file extension. Falls back to
/// `application/octet-stream`, which is always a valid answer.
pub fn content_type_for(path: &DavPath) -> String {
    path.name()
        .and_then(|name| mime_guess::from_path(name).first())
        .map(|m| m.essence_str().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Percent-encode a `DavPath` into an href, with a trailing slash for
/// collections (both mirall and iOS's WebDAV client rely on the trailing
/// slash to tell collections and files apart in a PROPFIND response).
pub fn encode_href(path: &DavPath, is_collection: bool) -> String {
    let mut out = String::new();
    for seg in path.segments() {
        out.push('/');
        out.push_str(&utf8_percent_encode(seg, PATH_ENCODE_SET).to_string());
    }
    if out.is_empty() || is_collection {
        out.push('/');
    }
    out
}

/// Percent-encode a single path segment for use in a relative link (the
/// plain HTML directory listing on GET).
pub fn encode_segment(name: &str, is_collection: bool) -> String {
    let mut out = utf8_percent_encode(name, PATH_ENCODE_SET).to_string();
    if is_collection {
        out.push('/');
    }
    out
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use breadcloud_storage::EntryKind;
    use std::time::UNIX_EPOCH;

    fn file_entry(path: &str) -> ResourceEntry {
        ResourceEntry {
            path: DavPath::parse(path).unwrap(),
            metadata: Metadata {
                kind: EntryKind::File,
                len: 42,
                modified: UNIX_EPOCH,
            },
            quota: None,
        }
    }

    fn dir_entry(path: &str, quota: Option<Quota>) -> ResourceEntry {
        ResourceEntry {
            path: DavPath::parse(path).unwrap(),
            metadata: Metadata {
                kind: EntryKind::Directory,
                len: 0,
                modified: UNIX_EPOCH,
            },
            quota,
        }
    }

    #[test]
    fn file_href_has_no_trailing_slash() {
        let entries = vec![file_entry("/docs/a.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp);
        assert!(xml.contains("<d:href>/docs/a.txt</d:href>"));
        assert!(xml.contains("<d:getcontentlength>42</d:getcontentlength>"));
        assert!(xml.contains("<d:resourcetype/>"));
    }

    #[test]
    fn directory_href_has_trailing_slash_and_collection_type() {
        let entries = vec![dir_entry(
            "/docs",
            Some(Quota {
                used_bytes: 100,
                available_bytes: 900,
            }),
        )];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp);
        assert!(xml.contains("<d:href>/docs/</d:href>"));
        assert!(xml.contains("<d:resourcetype><d:collection/></d:resourcetype>"));
        assert!(xml.contains("<d:quota-used-bytes>100</d:quota-used-bytes>"));
        assert!(xml.contains("<d:quota-available-bytes>900</d:quota-available-bytes>"));
        assert!(!xml.contains("getcontentlength"));
    }

    #[test]
    fn unsupported_requested_prop_gets_its_own_404_propstat() {
        let entries = vec![file_entry("/docs/a.txt")];
        let request = PropfindRequest::Prop(vec!["getetag".to_string(), "fileid".to_string()]);
        let xml = build_multistatus(&entries, &request);
        assert!(xml.contains("<d:getetag>"));
        assert!(xml.contains("<d:fileid/>"));
        assert!(xml.contains("404 Not Found"));
        assert!(xml.contains("200 OK"));
    }

    #[test]
    fn href_percent_encodes_reserved_characters() {
        let entries = vec![file_entry("/a b#c.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp);
        assert!(xml.contains("<d:href>/a%20b%23c.txt</d:href>"));
    }

    #[test]
    fn displayname_escapes_xml_special_characters() {
        let entries = vec![file_entry("/a&b<c>.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp);
        assert!(xml.contains("<d:displayname>a&amp;b&lt;c&gt;.txt</d:displayname>"));
    }
}
