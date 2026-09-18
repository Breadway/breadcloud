use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

/// PROPFIND request-body parsing is genuinely the same problem for a
/// calendar collection as it is for a file/collection resource --
/// "which properties, by local name, ignoring namespace prefix, did the
/// client ask for" -- so this reuses `breadcloud_webdav`'s parser
/// directly rather than keeping a second copy of the same quick-xml
/// walk. The *property set* a PROPFIND response can answer differs a lot
/// between the two crates (see `crate::xml::response`); only the request
/// parsing is shared.
pub use breadcloud_webdav::xml::request::{parse_propfind_body, PropfindRequest};

/// Which of the two properties a `calendar-query`/`calendar-multiget`
/// REPORT's `<d:prop>` asked for. Real clients (DAVx5, iOS) always ask
/// for both; this is tracked anyway so a client that only wants etags
/// (a cheap "did anything change" pass) does not get a full
/// `calendar-data` body it didn't ask for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PropSet {
    pub getetag: bool,
    pub calendar_data: bool,
}

/// A `<c:time-range start=".." end=".."/>` filter's raw (still
/// iCalendar-format, not parsed further here) bounds. Either side may be
/// absent (an unbounded query in that direction).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimeRange {
    pub start: Option<String>,
    pub end: Option<String>,
}

/// A parsed REPORT request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportRequest {
    /// RFC 4791 7.9: fetch a known, explicit set of calendar objects by
    /// href.
    CalendarMultiget { hrefs: Vec<String>, props: PropSet },
    /// RFC 4791 7.8: fetch calendar objects matching a filter. This
    /// server supports the shape real clients actually send -- an
    /// optional inner `comp-filter` naming a component type
    /// (`VEVENT`/`VTODO`) and an optional `time-range` inside it -- not
    /// the full general filter grammar RFC 4791 9.7 defines (property
    /// text-match filters, nested `AND`/`OR` combinations of multiple
    /// component filters, and so on), which DAVx5 and iOS's Calendar app
    /// do not send in practice.
    CalendarQuery {
        component: Option<String>,
        time_range: Option<TimeRange>,
        props: PropSet,
    },
    /// RFC 6578: incremental sync. `sync_token = None` means an initial
    /// sync (no token presented yet).
    SyncCollection {
        sync_token: Option<String>,
        props: PropSet,
    },
    /// A REPORT this server does not recognize. Handled the same way an
    /// unsupported PROPFIND property is: a clear error response, not a
    /// panic or a silently-empty result.
    Unsupported,
}

/// Parse a REPORT request body into one of the three shapes this server
/// understands. Namespace-agnostic (local element names only), same
/// convention as `parse_propfind_body`: real clients bind `DAV:` and
/// `urn:ietf:params:xml:ns:caldav` to varying prefixes (`d:`/`c:`,
/// `cal:`, no prefix with a default xmlns, and so on).
///
/// This is a flat scan, not a structurally-validating nested parser: it
/// picks up the first `comp-filter` whose `name` is not `VCALENDAR` and
/// the first `time-range` seen anywhere in the body. Real clients always
/// send the canonical `VCALENDAR > VEVENT|VTODO [> time-range]` shape RFC
/// 4791's own examples use, so a flat scan finds exactly the same
/// element a structural parser would, with much less code -- see the
/// module-level scope note in `crate::ical` for the same "match what real
/// clients actually send" tradeoff made there.
pub fn parse_report_body(body: &[u8]) -> ReportRequest {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut root: Option<String> = None;
    let mut hrefs: Vec<String> = Vec::new();
    let mut props = PropSet::default();
    let mut sync_token: Option<String> = None;
    let mut component: Option<String> = None;
    let mut time_range: Option<TimeRange> = None;

    let mut in_prop = false;
    let mut in_href = false;
    let mut in_sync_token = false;
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
                    "comp-filter" => capture_comp_filter(&e, &mut component),
                    _ if in_prop => mark_prop(&local, &mut props),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "time-range" if time_range.is_none() => {
                        time_range = Some(TimeRange {
                            start: attr(&e, "start"),
                            end: attr(&e, "end"),
                        });
                    }
                    "comp-filter" => capture_comp_filter(&e, &mut component),
                    _ if in_prop => mark_prop(&local, &mut props),
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if in_href || in_sync_token {
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
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    match root.as_deref() {
        Some("calendar-multiget") => ReportRequest::CalendarMultiget { hrefs, props },
        Some("calendar-query") => ReportRequest::CalendarQuery {
            component,
            time_range,
            props,
        },
        Some("sync-collection") => ReportRequest::SyncCollection { sync_token, props },
        _ => ReportRequest::Unsupported,
    }
}

fn capture_comp_filter(e: &BytesStart, component: &mut Option<String>) {
    if let Some(name) = attr(e, "name") {
        if !name.eq_ignore_ascii_case("VCALENDAR") {
            *component = Some(name);
        }
    }
}

fn mark_prop(local: &str, props: &mut PropSet) {
    match local {
        "getetag" => props.getetag = true,
        "calendar-data" => props.calendar_data = true,
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

/// Parse a `MKCALENDAR` request body's `<d:set><d:prop>...` for the two
/// properties a client sometimes sets at creation time: `displayname` and
/// `calendar-color`. A body-less MKCALENDAR is valid too (DAVx5 lets a
/// user name a calendar client-side without always sending it in the
/// request) -- `(None, None)` in that case; the caller falls back to the
/// calendar id as its display name.
pub fn parse_mkcalendar_body(body: &[u8]) -> (Option<String>, Option<String>) {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return (None, None);
    }

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut display_name: Option<String> = None;
    let mut color: Option<String> = None;
    let mut current: Option<&'static str> = None;
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                match local.as_str() {
                    "displayname" => {
                        current = Some("displayname");
                        text_buf.clear();
                    }
                    "calendar-color" => {
                        current = Some("calendar-color");
                        text_buf.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if current.is_some() {
                    if let Ok(txt) = t.unescape() {
                        text_buf.push_str(&txt);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if Some(local.as_str()) == current {
                    let value = text_buf.trim().to_string();
                    if !value.is_empty() {
                        match current {
                            Some("displayname") => display_name = Some(value),
                            Some("calendar-color") => color = Some(value),
                            _ => {}
                        }
                    }
                    current = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    (display_name, color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calendar_multiget() {
        let body = br#"<?xml version="1.0"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <d:href>/remote.php/dav/calendars/dev/personal/one.ics</d:href>
  <d:href>/remote.php/dav/calendars/dev/personal/two.ics</d:href>
</c:calendar-multiget>"#;
        match parse_report_body(body) {
            ReportRequest::CalendarMultiget { hrefs, props } => {
                assert_eq!(
                    hrefs,
                    vec![
                        "/remote.php/dav/calendars/dev/personal/one.ics",
                        "/remote.php/dav/calendars/dev/personal/two.ics"
                    ]
                );
                assert!(props.getetag && props.calendar_data);
            }
            other => panic!("expected CalendarMultiget, got {other:?}"),
        }
    }

    #[test]
    fn parses_calendar_query_with_time_range() {
        let body = br#"<?xml version="1.0"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="20260101T000000Z" end="20260201T000000Z"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;
        match parse_report_body(body) {
            ReportRequest::CalendarQuery {
                component,
                time_range,
                props,
            } => {
                assert_eq!(component.as_deref(), Some("VEVENT"));
                let tr = time_range.unwrap();
                assert_eq!(tr.start.as_deref(), Some("20260101T000000Z"));
                assert_eq!(tr.end.as_deref(), Some("20260201T000000Z"));
                assert!(props.getetag && props.calendar_data);
            }
            other => panic!("expected CalendarQuery, got {other:?}"),
        }
    }

    #[test]
    fn parses_calendar_query_without_time_range() {
        let body = br#"<?xml version="1.0"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VTODO"/></c:comp-filter></c:filter>
</c:calendar-query>"#;
        match parse_report_body(body) {
            ReportRequest::CalendarQuery {
                component,
                time_range,
                props,
            } => {
                assert_eq!(component.as_deref(), Some("VTODO"));
                assert!(time_range.is_none());
                assert!(props.getetag && !props.calendar_data);
            }
            other => panic!("expected CalendarQuery, got {other:?}"),
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
  <d:sync-level>1</d:sync-level>
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
    fn parses_mkcalendar_body_displayname_and_color() {
        let body = br#"<?xml version="1.0"?>
<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:ical="http://apple.com/ns/ical/">
  <d:set>
    <d:prop>
      <d:displayname>Trips</d:displayname>
      <ical:calendar-color>#0000FF</ical:calendar-color>
    </d:prop>
  </d:set>
</c:mkcalendar>"#;
        let (name, color) = parse_mkcalendar_body(body);
        assert_eq!(name.as_deref(), Some("Trips"));
        assert_eq!(color.as_deref(), Some("#0000FF"));
    }

    #[test]
    fn empty_mkcalendar_body_is_none_none() {
        assert_eq!(parse_mkcalendar_body(b""), (None, None));
    }
}
