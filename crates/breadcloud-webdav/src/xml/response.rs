use breadcloud_storage::{DavPath, Metadata, Quota};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use super::request::{PropfindRequest, RequestedProperty};
use std::collections::BTreeMap;

const DAV_NAMESPACE: &str = "DAV:";
const OWNCLOUD_NAMESPACE: &str = "http://owncloud.org/ns";

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
    /// The etag to report, precomputed by the caller (via
    /// `Storage::etag`) rather than derived here: for a file this is a
    /// content-derived hash, which `Metadata` alone cannot produce
    /// without an async storage lookup.
    pub etag: String,
    /// `Some` only for the collections we want to report quota on
    /// (currently: any directory, computed from the filesystem backing
    /// the storage root -- see [`breadcloud_storage::Storage::quota`]).
    pub quota: Option<Quota>,
}

/// Build a full `multistatus` XML document for a PROPFIND response.
///
/// `prefix` is prepended to every href verbatim (no extra slash inserted):
/// real Nextcloud clients mount the tree at
/// `/remote.php/dav/files/<user>` and reject a response whose hrefs don't
/// start with that exact prefix, so it must be echoed back, not just the
/// storage-relative path. Pass `""` for a bare root mount (this crate's
/// own tests, or a non-Nextcloud client like rclone/davfs2).
pub fn build_multistatus(
    resources: &[ResourceEntry],
    request: &PropfindRequest,
    prefix: &str,
) -> String {
    let extension_prefixes = requested_namespace_prefixes(request);
    let mut body = String::new();
    body.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    body.push_str("<d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\"");
    for (namespace, prefix) in &extension_prefixes {
        body.push_str(&format!(
            " xmlns:{prefix}=\"{}\"",
            xml_escape_attribute(namespace)
        ));
    }
    body.push_str(">\n");

    for entry in resources {
        body.push_str("  <d:response>\n");
        body.push_str("    <d:href>");
        body.push_str(&xml_escape(&encode_href(
            &entry.path,
            entry.metadata.is_dir(),
            prefix,
        )));
        body.push_str("</d:href>\n");

        match request {
            PropfindRequest::PropName => {
                let applicable = applicable_props(entry);
                body.push_str("    <d:propstat>\n      <d:prop>\n");
                for name in applicable {
                    body.push_str(&format!("        <{}:{name}/>\n", prop_namespace(name)));
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
                let is_applicable = |property: &RequestedProperty| {
                    let expected_namespace = if prop_namespace(&property.local_name) == "oc" {
                        OWNCLOUD_NAMESPACE
                    } else {
                        DAV_NAMESPACE
                    };
                    property.namespace.as_deref() == Some(expected_namespace)
                        && applicable.contains(&property.local_name.as_str())
                };

                let found: Vec<&str> = requested
                    .iter()
                    .filter(|property| is_applicable(property))
                    .map(|property| property.local_name.as_str())
                    .collect();
                let missing: Vec<&RequestedProperty> = requested
                    .iter()
                    .filter(|property| !is_applicable(property))
                    .collect();

                if !found.is_empty() {
                    body.push_str("    <d:propstat>\n      <d:prop>\n");
                    body.push_str(&render_prop_values(entry, &found));
                    body.push_str("      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n");
                }
                if !missing.is_empty() {
                    body.push_str("    <d:propstat>\n      <d:prop>\n");
                    for property in missing {
                        body.push_str(&format!(
                            "        <{}/>\n",
                            property_qualified_name(property, &extension_prefixes)
                        ));
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

/// Nextcloud's own extension properties (`oc:fileid`, `oc:permissions`,
/// and so on) live in the `http://owncloud.org/ns` namespace, bound to
/// the `oc:` prefix on the multistatus root; everything this server
/// understands otherwise is a plain WebDAV property in `DAV:` (`d:`).
fn prop_namespace(name: &str) -> &'static str {
    match name {
        "fileid" | "id" | "permissions" => "oc",
        _ => "d",
    }
}

fn requested_namespace_prefixes(request: &PropfindRequest) -> BTreeMap<String, String> {
    let PropfindRequest::Prop(properties) = request else {
        return BTreeMap::new();
    };

    let mut prefixes = BTreeMap::new();
    for property in properties {
        let Some(namespace) = property.namespace.as_ref() else {
            continue;
        };
        if namespace == DAV_NAMESPACE || namespace == OWNCLOUD_NAMESPACE {
            continue;
        }
        if !prefixes.contains_key(namespace) {
            let prefix = format!("p{}", prefixes.len());
            prefixes.insert(namespace.clone(), prefix);
        }
    }
    prefixes
}

fn property_qualified_name(
    property: &RequestedProperty,
    extension_prefixes: &BTreeMap<String, String>,
) -> String {
    match property.namespace.as_deref() {
        Some(DAV_NAMESPACE) => format!("d:{}", property.local_name),
        Some(OWNCLOUD_NAMESPACE) => format!("oc:{}", property.local_name),
        Some(namespace) => extension_prefixes
            .get(namespace)
            .map(|prefix| format!("{prefix}:{}", property.local_name))
            .unwrap_or_else(|| property.local_name.clone()),
        None => property.local_name.clone(),
    }
}

/// Properties we can actually answer for this specific resource (e.g.
/// `getcontentlength` only makes sense for a file, quota only for a
/// collection whose quota we computed).
///
/// `fileid`, `id`, and `permissions` are always applicable: confirmed
/// against the real Nextcloud desktop client that a second sync pass
/// (not just the initial upload) requires all three on every entry, or
/// it marks the whole item "not accessible on the server" and refuses to
/// proceed, regardless of getetag/getcontentlength/displayname all being
/// present and correct. mirall's PROPFIND requests both `oc:id` and
/// `oc:fileid` (observed) and reads its internal file-id field from
/// `oc:id`, not `oc:fileid` -- both get the same value here since real
/// Nextcloud reports the same id under both names too.
fn applicable_props(entry: &ResourceEntry) -> Vec<&'static str> {
    let mut props = vec![
        "resourcetype",
        "getetag",
        "getlastmodified",
        "displayname",
        "fileid",
        "id",
        "permissions",
    ];
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
                out.push_str(&xml_escape(&entry.etag));
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
            "fileid" => {
                out.push_str("        <oc:fileid>");
                out.push_str(&xml_escape(&crate::fileid::for_path(&entry.path)));
                out.push_str("</oc:fileid>\n");
            }
            "id" => {
                // Same value as fileid: see this function's doc comment.
                out.push_str("        <oc:id>");
                out.push_str(&xml_escape(&crate::fileid::for_path(&entry.path)));
                out.push_str("</oc:id>\n");
            }
            "permissions" => {
                // This server has one owner-equivalent user (see
                // breadcloud_webdav::auth) with full rights over
                // everything until CLOUD-11 adds real multi-user
                // accounts, so every resource gets the same generous
                // permission set: Read, delete (D), reNamable/move (NV),
                // Write (W), and for a collection also Create files (C)
                // and mKdir (K).
                let perms = if entry.metadata.is_dir() {
                    "RGDNVCK"
                } else {
                    "RGDNVW"
                };
                out.push_str("        <oc:permissions>");
                out.push_str(perms);
                out.push_str("</oc:permissions>\n");
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
/// slash to tell collections and files apart in a PROPFIND response), and
/// `prefix` prepended verbatim (see [`build_multistatus`]).
pub fn encode_href(path: &DavPath, is_collection: bool, prefix: &str) -> String {
    let mut out = String::from(prefix);
    for seg in path.segments() {
        out.push('/');
        out.push_str(&utf8_percent_encode(seg, PATH_ENCODE_SET).to_string());
    }
    if out.is_empty() || out == prefix {
        out.push('/');
    }
    if is_collection && !out.ends_with('/') {
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

fn xml_escape_attribute(input: &str) -> String {
    // Literal attribute whitespace would be normalized to spaces by clients.
    // References retain the original characters in the namespace's URI.
    xml_escape(input)
        .replace('\t', "&#9;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
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
            etag: "\"deadbeef\"".to_string(),
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
            etag: "\"cafef00d\"".to_string(),
            quota,
        }
    }

    #[test]
    fn file_href_has_no_trailing_slash() {
        let entries = vec![file_entry("/docs/a.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
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
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
        assert!(xml.contains("<d:href>/docs/</d:href>"));
        assert!(xml.contains("<d:resourcetype><d:collection/></d:resourcetype>"));
        assert!(xml.contains("<d:quota-used-bytes>100</d:quota-used-bytes>"));
        assert!(xml.contains("<d:quota-available-bytes>900</d:quota-available-bytes>"));
        assert!(!xml.contains("getcontentlength"));
    }

    #[test]
    fn unsupported_requested_prop_gets_its_own_404_propstat() {
        let entries = vec![file_entry("/docs/a.txt")];
        // share-types is a real oc: property this server does not
        // implement (there is no sharing yet); fileid and permissions
        // are both supported (see the tests below) so they no longer
        // belong in this list.
        let request = PropfindRequest::Prop(vec![
            RequestedProperty {
                namespace: Some(DAV_NAMESPACE.to_string()),
                local_name: "getetag".to_string(),
            },
            RequestedProperty {
                namespace: Some(OWNCLOUD_NAMESPACE.to_string()),
                local_name: "share-types".to_string(),
            },
        ]);
        let xml = build_multistatus(&entries, &request, "");
        assert!(xml.contains("<d:getetag>"));
        assert!(xml.contains("<oc:share-types/>"));
        assert!(xml.contains("404 Not Found"));
        assert!(xml.contains("200 OK"));
    }

    #[test]
    fn unsupported_requested_prop_keeps_its_namespace() {
        let entries = vec![file_entry("/docs/a.txt")];
        let request = PropfindRequest::Prop(vec![RequestedProperty {
            namespace: Some("http://nextcloud.org/ns".to_string()),
            local_name: "has-preview".to_string(),
        }]);
        let xml = build_multistatus(&entries, &request, "");
        assert!(xml.contains("xmlns:p0=\"http://nextcloud.org/ns\""));
        assert!(xml.contains("<p0:has-preview/>"));
        assert!(xml.contains("404 Not Found"));
    }

    #[test]
    fn escaped_extension_namespace_round_trips_its_expanded_name() {
        let request = super::super::request::parse_propfind_body(
            br#"<d:propfind xmlns:d="DAV:" xmlns:x="urn:test?a&amp;b&#9;c"><d:prop><x:custom/></d:prop></d:propfind>"#,
        );
        let xml = build_multistatus(&[file_entry("/a.txt")], &request, "");
        let mut reader = quick_xml::reader::NsReader::from_str(&xml);
        let mut found = false;
        loop {
            let (namespace, event) = reader.read_resolved_event().unwrap();
            match event {
                quick_xml::events::Event::Empty(element)
                    if element.local_name().as_ref() == b"custom" =>
                {
                    let quick_xml::name::ResolveResult::Bound(namespace) = namespace else {
                        panic!("extension property has no namespace");
                    };
                    let raw = std::str::from_utf8(namespace.0).unwrap();
                    assert_eq!(quick_xml::escape::unescape(raw).unwrap(), "urn:test?a&b\tc");
                    assert!(!raw.contains('\t'));
                    found = true;
                }
                quick_xml::events::Event::Eof => break,
                _ => {}
            }
        }
        assert!(found);
        assert!(xml.contains("404 Not Found"));
    }

    #[test]
    fn character_reference_dav_namespace_selects_supported_property() {
        let request = super::super::request::parse_propfind_body(
            br#"<d:propfind xmlns:d="DAV&#58;"><d:prop><d:getetag/></d:prop></d:propfind>"#,
        );
        let xml = build_multistatus(&[file_entry("/a.txt")], &request, "");
        assert!(xml.contains("<d:getetag>&quot;deadbeef&quot;</d:getetag>"));
        assert!(xml.contains("HTTP/1.1 200 OK"));
        assert!(!xml.contains("404 Not Found"));
    }

    #[test]
    fn unsupported_requested_props_with_multiple_namespaces_are_declared() {
        let entries = vec![file_entry("/docs/a.txt")];
        let request = PropfindRequest::Prop(vec![
            RequestedProperty {
                namespace: Some("urn:example:first".to_string()),
                local_name: "alpha".to_string(),
            },
            RequestedProperty {
                namespace: Some("urn:example:second".to_string()),
                local_name: "beta".to_string(),
            },
        ]);
        let xml = build_multistatus(&entries, &request, "");
        assert!(xml.contains("xmlns:p0=\"urn:example:first\""));
        assert!(xml.contains("xmlns:p1=\"urn:example:second\""));
        assert!(xml.contains("<p0:alpha/>"));
        assert!(xml.contains("<p1:beta/>"));
    }

    #[test]
    fn fileid_and_permissions_are_always_reported() {
        // A real Nextcloud desktop client's *second* sync pass (not just
        // the initial upload) requires all of these on every entry or it
        // marks the item "not accessible on the server" -- confirmed
        // against nextcloudcmd. It requests both oc:id and oc:fileid but
        // reads its internal file-id field from oc:id.
        let file = vec![file_entry("/docs/a.txt")];
        let xml = build_multistatus(&file, &PropfindRequest::AllProp, "");
        assert!(xml.contains("<oc:fileid>"));
        assert!(xml.contains("<oc:id>"));
        assert!(xml.contains("<oc:permissions>RGDNVW</oc:permissions>"));

        let dir = vec![dir_entry("/docs", None)];
        let xml = build_multistatus(&dir, &PropfindRequest::AllProp, "");
        assert!(xml.contains("<oc:fileid>"));
        assert!(xml.contains("<oc:id>"));
        assert!(xml.contains("<oc:permissions>RGDNVCK</oc:permissions>"));
    }

    #[test]
    fn fileid_is_stable_for_the_same_path() {
        let a = vec![file_entry("/docs/a.txt")];
        let b = vec![file_entry("/docs/a.txt")];
        let xml_a = build_multistatus(&a, &PropfindRequest::AllProp, "");
        let xml_b = build_multistatus(&b, &PropfindRequest::AllProp, "");
        let extract = |xml: &str| {
            let start = xml.find("<oc:fileid>").unwrap() + "<oc:fileid>".len();
            let end = xml[start..].find("</oc:fileid>").unwrap() + start;
            xml[start..end].to_string()
        };
        assert_eq!(extract(&xml_a), extract(&xml_b));
    }

    #[test]
    fn multistatus_root_declares_the_oc_namespace() {
        let entries = vec![file_entry("/a.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
        assert!(xml.contains("xmlns:oc=\"http://owncloud.org/ns\""));
    }

    #[test]
    fn href_percent_encodes_reserved_characters() {
        let entries = vec![file_entry("/a b#c.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
        assert!(xml.contains("<d:href>/a%20b%23c.txt</d:href>"));
    }

    #[test]
    fn href_carries_the_dav_mount_prefix_real_clients_require() {
        // Real Nextcloud clients (mirall) mount at
        // /remote.php/dav/files/<user> and reject a PROPFIND response
        // whose hrefs don't start with that exact prefix.
        let prefix = "/remote.php/dav/files/dev";
        let file = vec![file_entry("/docs/a.txt")];
        let xml = build_multistatus(&file, &PropfindRequest::AllProp, prefix);
        assert!(xml.contains("<d:href>/remote.php/dav/files/dev/docs/a.txt</d:href>"));

        let root = vec![dir_entry("/", None)];
        let xml = build_multistatus(&root, &PropfindRequest::AllProp, prefix);
        assert!(xml.contains("<d:href>/remote.php/dav/files/dev/</d:href>"));

        let dir = vec![dir_entry("/docs", None)];
        let xml = build_multistatus(&dir, &PropfindRequest::AllProp, prefix);
        assert!(xml.contains("<d:href>/remote.php/dav/files/dev/docs/</d:href>"));
    }

    #[test]
    fn getetag_uses_the_precomputed_entry_etag() {
        let entries = vec![file_entry("/a.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
        // The quotes in the etag get XML-escaped like any other content.
        assert!(xml.contains("<d:getetag>&quot;deadbeef&quot;</d:getetag>"));
    }

    #[test]
    fn displayname_escapes_xml_special_characters() {
        let entries = vec![file_entry("/a&b<c>.txt")];
        let xml = build_multistatus(&entries, &PropfindRequest::AllProp, "");
        assert!(xml.contains("<d:displayname>a&amp;b&lt;c&gt;.txt</d:displayname>"));
    }
}
