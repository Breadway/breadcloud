use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// PROPFIND request-body parsing is the same problem for an addressbook
/// collection as for a file or calendar one -- see
/// `breadcloud_caldav::xml::request`'s identical re-export for the same
/// reasoning. Only the request parsing is shared; the *response* property
/// set is CardDAV's own (see `crate::xml::response`).
pub use breadcloud_webdav::xml::request::{parse_propfind_body, PropfindRequest};

/// Which of the two properties an `addressbook-query`/`addressbook-multiget`
/// REPORT's `<d:prop>` asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropSet {
    pub getetag: bool,
    pub address_data: bool,
}

/// A single `prop-filter`/`text-match` pair from an `addressbook-query`
/// filter (RFC 6352 8.6.4), e.g. `<c:prop-filter name="FN"><c:text-match>
/// ada</c:text-match></c:prop-filter>`. This server supports the shape
/// real clients actually send -- at most one property filter with at most
/// one text-match -- not RFC 6352's full filter grammar (multiple
/// prop-filters combined with `test="anyof"/"allof"`, `param-filter`,
/// negated matches), the same "match what real clients send, not the
/// whole grammar" tradeoff `breadcloud_caldav`'s own REPORT parser
/// documents for `calendar-query`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropFilter {
    pub name: String,
    pub text: String,
}

/// A parsed REPORT request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportRequest {
    /// RFC 6352 8.7: fetch a known, explicit set of contact objects by
    /// href.
    AddressbookMultiget { hrefs: Vec<String>, props: PropSet },
    /// RFC 6352 8.6: fetch contact objects matching an optional
    /// property text-match filter. `filter = None` means "every contact
    /// in the addressbook" (no `<c:filter>` element, or one with no
    /// `prop-filter` inside it).
    AddressbookQuery {
        filter: Option<PropFilter>,
        props: PropSet,
    },
    /// RFC 6578: incremental sync, identical semantics to
    /// `breadcloud_caldav`'s own `sync-collection` handling.
    SyncCollection {
        sync_token: Option<String>,
        props: PropSet,
    },
    /// A REPORT this server does not recognize.
    Unsupported,
}

/// Parse a REPORT request body into one of the three shapes this server
/// understands. Namespace-agnostic (local element names only) and a flat
/// scan rather than a structurally-validating nested parser, same
/// convention and same tradeoff as
/// `breadcloud_caldav::xml::request::parse_report_body`.
pub fn parse_report_body(body: &[u8]) -> ReportRequest {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut root: Option<String> = None;
    let mut hrefs: Vec<String> = Vec::new();
    let mut props = PropSet::default();
    let mut sync_token: Option<String> = None;
    let mut filter_name: Option<String> = None;
    let mut filter_text: Option<String> = None;

    let mut in_prop = false;
    let mut in_href = false;
    let mut in_sync_token = false;
    let mut in_text_match = false;
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if root.is_none() {
                    root = Some(local.clone());
                }
                match local.as_str() {
                    "prop" => in_prop = true,
                    "href" => {
                        in_href = true;
                        text_buf.clear();
                    }
                    "sync-token" => {
                        in_sync_token = true;
                        text_buf.clear();
                    }
                    "prop-filter" if filter_name.is_none() => {
                        filter_name = attr(&e, "name");
                    }
                    "text-match" if filter_name.is_some() && filter_text.is_none() => {
                        in_text_match = true;
                        text_buf.clear();
                    }
                    _ if in_prop => mark_prop(&local, &mut props),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "prop-filter" if filter_name.is_none() => {
                        filter_name = attr(&e, "name");
                    }
                    _ if in_prop => mark_prop(&local, &mut props),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_href || in_sync_token || in_text_match {
                    if let Ok(txt) = t.unescape() {
                        text_buf.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "prop" => in_prop = false,
                    "href" => {
                        in_href = false;
                        let href = text_buf.trim();
                        if !href.is_empty() {
                            hrefs.push(href.to_string());
                        }
                    }
                    "sync-token" => {
                        in_sync_token = false;
                        let token = text_buf.trim();
                        if !token.is_empty() {
                            sync_token = Some(token.to_string());
                        }
                    }
                    "text-match" => {
                        in_text_match = false;
                        filter_text = Some(text_buf.trim().to_string());
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    let filter = match (filter_name, filter_text) {
        (Some(name), Some(text)) => Some(PropFilter { name, text }),
        (Some(name), None) => Some(PropFilter {
            name,
            text: String::new(),
        }),
        _ => None,
    };

    match root.as_deref() {
        Some("addressbook-multiget") => ReportRequest::AddressbookMultiget { hrefs, props },
        Some("addressbook-query") => ReportRequest::AddressbookQuery { filter, props },
        Some("sync-collection") => ReportRequest::SyncCollection { sync_token, props },
        _ => ReportRequest::Unsupported,
    }
}

fn mark_prop(local: &str, props: &mut PropSet) {
    match local {
        "getetag" => props.getetag = true,
        "address-data" => props.address_data = true,
        _ => {}
    }
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if local_name(a.key.as_ref()) == name {
            a.unescape_value().ok().map(|v| v.into_owned())
        } else {
            None
        }
    })
}

fn local_name(qname: &[u8]) -> String {
    let s = std::str::from_utf8(qname).unwrap_or("");
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

/// Parse an (extended, RFC 5689) `MKCOL` request body's `<d:set><d:prop>`
/// for `displayname`. CardDAV has no `MKCALENDAR`-equivalent verb of its
/// own (RFC 6352 does not define one): an addressbook is created with a
/// plain `MKCOL`, optionally carrying an RFC 5689 extended body that also
/// sets `resourcetype` (to include `<card:addressbook/>`) and
/// `displayname` in the same request. This server does not require the
/// extended body or validate the `resourcetype` it sets -- every `MKCOL`
/// under `/remote.php/dav/addressbooks/<user>/` unambiguously means
/// "create an addressbook" regardless of body, since that is the only
/// kind of collection this path ever holds (see
/// `handlers::mkcol::handle`) -- it just reads `displayname` out of the
/// body if present. A body-less (plain RFC 4918) `MKCOL` is valid too:
/// `(None)` in that case, and the caller falls back to the addressbook id
/// as its display name, matching `parse_mkcalendar_body`'s own fallback.
pub fn parse_mkcol_body(body: &[u8]) -> Option<String> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return None;
    }

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut display_name: Option<String> = None;
    let mut in_displayname = false;
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == "displayname" {
                    in_displayname = true;
                    text_buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if in_displayname {
                    if let Ok(txt) = t.unescape() {
                        text_buf.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == "displayname" && in_displayname {
                    in_displayname = false;
                    let value = text_buf.trim().to_string();
                    if !value.is_empty() {
                        display_name = Some(value);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    display_name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addressbook_multiget() {
        let body = br#"<?xml version="1.0"?>
<c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <d:href>/remote.php/dav/addressbooks/dev/contacts/one.vcf</d:href>
  <d:href>/remote.php/dav/addressbooks/dev/contacts/two.vcf</d:href>
</c:addressbook-multiget>"#;
        match parse_report_body(body) {
            ReportRequest::AddressbookMultiget { hrefs, props } => {
                assert_eq!(
                    hrefs,
                    vec![
                        "/remote.php/dav/addressbooks/dev/contacts/one.vcf",
                        "/remote.php/dav/addressbooks/dev/contacts/two.vcf"
                    ]
                );
                assert!(props.getetag && props.address_data);
            }
            other => panic!("expected AddressbookMultiget, got {other:?}"),
        }
    }

    #[test]
    fn parses_addressbook_query_with_text_match_filter() {
        let body = br#"<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <c:filter>
    <c:prop-filter name="FN">
      <c:text-match>Ada</c:text-match>
    </c:prop-filter>
  </c:filter>
</c:addressbook-query>"#;
        match parse_report_body(body) {
            ReportRequest::AddressbookQuery { filter, props } => {
                let f = filter.unwrap();
                assert_eq!(f.name, "FN");
                assert_eq!(f.text, "Ada");
                assert!(props.getetag && props.address_data);
            }
            other => panic!("expected AddressbookQuery, got {other:?}"),
        }
    }

    #[test]
    fn parses_addressbook_query_without_filter() {
        let body = br#"<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/></d:prop>
  <c:filter/>
</c:addressbook-query>"#;
        match parse_report_body(body) {
            ReportRequest::AddressbookQuery { filter, props } => {
                assert!(filter.is_none());
                assert!(props.getetag && !props.address_data);
            }
            other => panic!("expected AddressbookQuery, got {other:?}"),
        }
    }

    #[test]
    fn parses_sync_collection_with_token() {
        let body = br#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>breadcloud-sync:5</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#;
        match parse_report_body(body) {
            ReportRequest::SyncCollection { sync_token, props } => {
                assert_eq!(sync_token.as_deref(), Some("breadcloud-sync:5"));
                assert!(props.getetag);
            }
            other => panic!("expected SyncCollection, got {other:?}"),
        }
    }

    #[test]
    fn parses_sync_collection_initial_request_with_no_token() {
        let body = br#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#;
        match parse_report_body(body) {
            ReportRequest::SyncCollection { sync_token, .. } => {
                assert_eq!(sync_token, None);
            }
            other => panic!("expected SyncCollection, got {other:?}"),
        }
    }

    #[test]
    fn unrecognized_root_is_unsupported() {
        let body = br#"<?xml version="1.0"?><d:expand-property xmlns:d="DAV:"/>"#;
        assert_eq!(parse_report_body(body), ReportRequest::Unsupported);
    }

    #[test]
    fn parses_extended_mkcol_body_displayname() {
        let body = br#"<?xml version="1.0"?>
<d:mkcol xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:set>
    <d:prop>
      <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
      <d:displayname>Contacts</d:displayname>
    </d:prop>
  </d:set>
</d:mkcol>"#;
        assert_eq!(parse_mkcol_body(body).as_deref(), Some("Contacts"));
    }

    #[test]
    fn empty_mkcol_body_is_none() {
        assert_eq!(parse_mkcol_body(b""), None);
    }
}
