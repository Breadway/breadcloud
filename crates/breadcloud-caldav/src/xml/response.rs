//! Building CalDAV `multistatus` XML.
//!
//! Deliberately not built on `breadcloud_webdav::xml::response`'s
//! `ResourceEntry`/`build_multistatus`: those are shaped around a file
//! tree's `Metadata` (kind, length, mtime) and a fixed WebDAV/Nextcloud
//! property set, and they precisely honor an exact requested-property
//! list (needed there because a real Nextcloud desktop client's second
//! sync pass depends on getting an exact 404 for a handful of specific
//! unsupported properties -- see that module's doc comment). Calendar
//! resources have none of that: a principal, a calendar-home-set
//! collection, a calendar collection, and a calendar object each have
//! their own small, fixed, genuinely different property set, and DAVx5 /
//! iOS's Calendar app do not depend on selective per-property 404s the
//! way mirall does. So this always answers the full property set this
//! server knows for a given resource kind, regardless of exactly which
//! properties were named in the request's `<d:prop>` (still honoring
//! `PropfindRequest::PropName` -- names only, no values -- since that
//! costs nothing extra to support). This is a scope simplification,
//! documented in docs/DESIGN.md too.

use crate::xml::request::{PropSet, PropfindRequest};

const PATH_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
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

pub fn encode_href(path: &str) -> String {
    path.split('/')
        .map(|seg| percent_encoding::utf8_percent_encode(seg, PATH_ENCODE_SET).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

pub fn escape(input: &str) -> String {
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

const NAMESPACES: &str = "xmlns:d=\"DAV:\" xmlns:cal=\"urn:ietf:params:xml:ns:caldav\" \
     xmlns:cs=\"http://calendarserver.org/ns/\" xmlns:ical=\"http://apple.com/ns/ical/\"";

fn multistatus_open(buf: &mut String) {
    buf.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    buf.push_str(&format!("<d:multistatus {NAMESPACES}>\n"));
}

fn multistatus_close(buf: &mut String) {
    buf.push_str("</d:multistatus>\n");
}

/// One `<d:prop>` entry this response knows how to render: its
/// (possibly-namespaced) tag, and its inner XML if any (empty string for
/// a valueless marker property like `<d:collection/>`'s container --
/// callers building `resourcetype` pass the whole inner markup here).
struct Prop {
    tag: &'static str,
    value: String,
}

fn render_response(buf: &mut String, href: &str, props: &[Prop], mode: &PropfindRequest) {
    buf.push_str("  <d:response>\n");
    buf.push_str("    <d:href>");
    buf.push_str(&escape(&encode_href(href)));
    buf.push_str("</d:href>\n");
    buf.push_str("    <d:propstat>\n      <d:prop>\n");
    for p in props {
        match mode {
            PropfindRequest::PropName => {
                buf.push_str(&format!("        <{}/>\n", p.tag));
            }
            _ => {
                if p.value.is_empty() {
                    buf.push_str(&format!("        <{}/>\n", p.tag));
                } else {
                    buf.push_str(&format!("        <{}>{}</{}>\n", p.tag, p.value, p.tag));
                }
            }
        }
    }
    buf.push_str(
        "      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n",
    );
    buf.push_str("  </d:response>\n");
}

/// PROPFIND response for `current-user-principal` discovery at the base
/// DAV URL (e.g. `/remote.php/dav/` or `/remote.php/dav`).
pub fn current_user_principal_multistatus(
    requested_href: &str,
    principal_href: &str,
    mode: &PropfindRequest,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    let props = vec![Prop {
        tag: "d:current-user-principal",
        value: href_elem(principal_href),
    }];
    render_response(&mut buf, requested_href, &props, mode);
    multistatus_close(&mut buf);
    buf
}

/// PROPFIND response for a user's principal resource
/// (`/remote.php/dav/principals/<user>/`).
pub fn principal_multistatus(
    principal_href: &str,
    home_href: &str,
    user: &str,
    mode: &PropfindRequest,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    let props = vec![
        Prop {
            tag: "d:resourcetype",
            value: "<d:principal/>".to_string(),
        },
        Prop {
            tag: "d:displayname",
            value: escape(user),
        },
        Prop {
            tag: "d:current-user-principal",
            value: href_elem(principal_href),
        },
        Prop {
            tag: "d:principal-URL",
            value: href_elem(principal_href),
        },
        Prop {
            tag: "cal:calendar-home-set",
            value: href_elem(home_href),
        },
        Prop {
            tag: "cal:calendar-user-address-set",
            value: href_elem(principal_href),
        },
    ];
    render_response(&mut buf, principal_href, &props, mode);
    multistatus_close(&mut buf);
    buf
}

/// One calendar collection to render inside a calendar-home-set listing.
pub struct CalendarEntry<'a> {
    pub href: String,
    pub display_name: &'a str,
    pub color: Option<&'a str>,
    pub ctag: &'a str,
}

/// PROPFIND response for `/remote.php/dav/calendars/<user>/` (Depth 0:
/// just the home-set collection; Depth 1: also every calendar under it).
pub fn calendar_home_multistatus(
    home_href: &str,
    calendars: &[CalendarEntry],
    include_children: bool,
    mode: &PropfindRequest,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);

    let home_props = vec![
        Prop {
            tag: "d:resourcetype",
            value: "<d:collection/>".to_string(),
        },
        Prop {
            tag: "d:displayname",
            value: "Calendars".to_string(),
        },
    ];
    render_response(&mut buf, home_href, &home_props, mode);

    if include_children {
        for cal in calendars {
            render_calendar_collection(&mut buf, cal, mode);
        }
    }

    multistatus_close(&mut buf);
    buf
}

/// PROPFIND response for a single calendar collection directly: Depth 0
/// is just the collection's own properties (`children = None`); Depth 1
/// also lists each event resource directly under it (`children =
/// Some(&[(href, etag)])`) -- the same "list this collection's immediate
/// members" idiom a plain WebDAV PROPFIND Depth 1 uses, which real
/// clients (vdirsyncer's CalDAV storage backend, confirmed live -- see
/// docs/DESIGN.md) rely on to enumerate a calendar's objects
/// alongside, or instead of, `REPORT calendar-query`/`sync-collection`.
/// Each child response only reports `getetag` (and an empty
/// `resourcetype`, marking it a non-collection resource) -- enough to
/// enumerate and detect changes; a client fetches actual content via GET
/// or `calendar-multiget`, the same division `breadcloud_webdav`'s own
/// PROPFIND/GET split uses for files.
pub fn calendar_collection_multistatus(
    cal: &CalendarEntry,
    children: Option<&[(String, String)]>,
    mode: &PropfindRequest,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    render_calendar_collection(&mut buf, cal, mode);
    if let Some(children) = children {
        for (href, etag) in children {
            let props = vec![
                Prop {
                    tag: "d:getetag",
                    value: escape(etag),
                },
                Prop {
                    tag: "d:resourcetype",
                    value: String::new(),
                },
            ];
            render_response(&mut buf, href, &props, mode);
        }
    }
    multistatus_close(&mut buf);
    buf
}

fn render_calendar_collection(buf: &mut String, cal: &CalendarEntry, mode: &PropfindRequest) {
    let mut props = vec![
        Prop {
            tag: "d:resourcetype",
            value: "<d:collection/><cal:calendar/>".to_string(),
        },
        Prop {
            tag: "d:displayname",
            value: escape(cal.display_name),
        },
        Prop {
            tag: "cs:getctag",
            value: escape(cal.ctag),
        },
        Prop {
            tag: "d:sync-token",
            value: escape(&breadcloud_storage::CalendarStore::format_sync_token(
                cal.ctag,
            )),
        },
        Prop {
            tag: "cal:supported-calendar-component-set",
            value: "<cal:comp name=\"VEVENT\"/><cal:comp name=\"VTODO\"/>".to_string(),
        },
    ];
    if let Some(color) = cal.color {
        props.push(Prop {
            tag: "ical:calendar-color",
            value: escape(color),
        });
    }
    render_response(buf, &cal.href, &props, mode);
}

fn href_elem(href: &str) -> String {
    format!("<d:href>{}</d:href>", escape(&encode_href(href)))
}

/// One calendar object (event) to render in a `calendar-query` /
/// `calendar-multiget` REPORT response.
pub struct EventEntry<'a> {
    pub href: String,
    pub etag: &'a str,
    pub ics: Option<&'a [u8]>,
}

/// REPORT response for `calendar-query`/`calendar-multiget`: one
/// `<d:response>` per matched/fetched event, each 200 OK with
/// `getetag`/`calendar-data` as requested.
pub fn calendar_object_multistatus(entries: &[EventEntry], props: &PropSet) -> String {
    calendar_object_multistatus_with_missing(entries, &[], props)
}

/// REPORT response for `calendar-multiget`, including a 404 response for
/// each requested href that could not be resolved.
pub fn calendar_object_multistatus_with_missing(
    entries: &[EventEntry],
    missing_hrefs: &[String],
    props: &PropSet,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    for entry in entries {
        let mut prop_list = Vec::new();
        if props.getetag || !props.calendar_data {
            prop_list.push(Prop {
                tag: "d:getetag",
                value: escape(entry.etag),
            });
        }
        if props.calendar_data {
            if let Some(ics) = entry.ics {
                prop_list.push(Prop {
                    tag: "cal:calendar-data",
                    value: escape(&String::from_utf8_lossy(ics)),
                });
            }
        }
        render_response(&mut buf, &entry.href, &prop_list, &PropfindRequest::AllProp);
    }
    for href in missing_hrefs {
        buf.push_str("  <d:response>\n    <d:href>");
        buf.push_str(&escape(&encode_href(href)));
        buf.push_str(
            "</d:href>\n    <d:status>HTTP/1.1 404 Not Found</d:status>\n  </d:response>\n",
        );
    }
    multistatus_close(&mut buf);
    buf
}

/// One entry in a `sync-collection` REPORT's diff.
pub struct SyncEntry<'a> {
    pub href: String,
    pub deleted: bool,
    pub etag: Option<&'a str>,
    pub ics: Option<&'a [u8]>,
}

/// REPORT response for `sync-collection` (RFC 6578): a 200 OK
/// `<d:response>` per changed resource (with the requested props), a 404
/// `<d:response>` per removed one, and the new `<d:sync-token>` the
/// client must present on its next incremental request.
pub fn sync_collection_multistatus(
    entries: &[SyncEntry],
    props: &PropSet,
    new_sync_token: &str,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    for entry in entries {
        if entry.deleted {
            buf.push_str("  <d:response>\n");
            buf.push_str("    <d:href>");
            buf.push_str(&escape(&encode_href(&entry.href)));
            buf.push_str("</d:href>\n");
            buf.push_str("    <d:status>HTTP/1.1 404 Not Found</d:status>\n");
            buf.push_str("  </d:response>\n");
            continue;
        }
        let mut prop_list = Vec::new();
        if let Some(etag) = entry.etag {
            prop_list.push(Prop {
                tag: "d:getetag",
                value: escape(etag),
            });
        }
        if props.calendar_data {
            if let Some(ics) = entry.ics {
                prop_list.push(Prop {
                    tag: "cal:calendar-data",
                    value: escape(&String::from_utf8_lossy(ics)),
                });
            }
        }
        render_response(&mut buf, &entry.href, &prop_list, &PropfindRequest::AllProp);
    }
    buf.push_str("  <d:sync-token>");
    buf.push_str(&escape(new_sync_token));
    buf.push_str("</d:sync-token>\n");
    multistatus_close(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_response_contains_calendar_home_set() {
        let xml = principal_multistatus(
            "/remote.php/dav/principals/dev/",
            "/remote.php/dav/calendars/dev/",
            "dev",
            &PropfindRequest::AllProp,
        );
        assert!(xml.contains("<cal:calendar-home-set>"));
        assert!(xml.contains("/remote.php/dav/calendars/dev/</d:href>"));
        assert!(xml.contains("<d:principal/>"));
    }

    #[test]
    fn calendar_home_lists_children_at_depth_1() {
        let calendars = vec![CalendarEntry {
            href: "/remote.php/dav/calendars/dev/personal/".to_string(),
            display_name: "Personal",
            color: Some("#ff0000"),
            ctag: "3",
        }];
        let xml = calendar_home_multistatus(
            "/remote.php/dav/calendars/dev/",
            &calendars,
            true,
            &PropfindRequest::AllProp,
        );
        assert!(xml.contains("<cal:calendar/>"));
        assert!(xml.contains("<cs:getctag>3</cs:getctag>"));
        assert!(xml.contains("<ical:calendar-color>#ff0000</ical:calendar-color>"));
        assert!(xml.contains("personal/</d:href>"));
    }

    #[test]
    fn calendar_home_omits_children_at_depth_0() {
        let calendars = vec![CalendarEntry {
            href: "/remote.php/dav/calendars/dev/personal/".to_string(),
            display_name: "Personal",
            color: None,
            ctag: "0",
        }];
        let xml = calendar_home_multistatus(
            "/remote.php/dav/calendars/dev/",
            &calendars,
            false,
            &PropfindRequest::AllProp,
        );
        assert!(!xml.contains("personal/</d:href>"));
    }

    #[test]
    fn calendar_object_response_includes_calendar_data_when_requested() {
        let entries = vec![EventEntry {
            href: "/remote.php/dav/calendars/dev/personal/uid1.ics".to_string(),
            etag: "\"abc\"",
            ics: Some(b"BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
        }];
        let props = PropSet {
            getetag: true,
            calendar_data: true,
        };
        let xml = calendar_object_multistatus(&entries, &props);
        assert!(xml.contains("<cal:calendar-data>BEGIN:VCALENDAR"));
        assert!(xml.contains("<d:getetag>&quot;abc&quot;</d:getetag>"));
    }

    #[test]
    fn sync_collection_response_marks_deletions_404_and_includes_token() {
        let entries = vec![
            SyncEntry {
                href: "/remote.php/dav/calendars/dev/personal/live.ics".to_string(),
                deleted: false,
                etag: Some("\"etag1\""),
                ics: None,
            },
            SyncEntry {
                href: "/remote.php/dav/calendars/dev/personal/gone.ics".to_string(),
                deleted: true,
                etag: None,
                ics: None,
            },
        ];
        let props = PropSet {
            getetag: true,
            calendar_data: false,
        };
        let xml = sync_collection_multistatus(&entries, &props, "breadcloud-sync:9");
        assert!(xml.contains("live.ics</d:href>"));
        assert!(xml.contains("gone.ics</d:href>\n    <d:status>HTTP/1.1 404 Not Found</d:status>"));
        assert!(xml.contains("<d:sync-token>breadcloud-sync:9</d:sync-token>"));
    }

    #[test]
    fn propname_mode_omits_values() {
        let calendars = vec![CalendarEntry {
            href: "/remote.php/dav/calendars/dev/personal/".to_string(),
            display_name: "Personal",
            color: None,
            ctag: "0",
        }];
        let xml = calendar_home_multistatus(
            "/remote.php/dav/calendars/dev/",
            &calendars,
            true,
            &PropfindRequest::PropName,
        );
        assert!(xml.contains("<cs:getctag/>"));
        assert!(!xml.contains("<cs:getctag>0</cs:getctag>"));
    }

    #[test]
    fn href_percent_encodes_reserved_characters() {
        assert_eq!(encode_href("/a b#c"), "/a%20b%23c");
    }

    #[test]
    fn calendar_collection_depth_0_has_no_children() {
        let cal = CalendarEntry {
            href: "/remote.php/dav/calendars/dev/personal/".to_string(),
            display_name: "Personal",
            color: None,
            ctag: "1",
        };
        let xml = calendar_collection_multistatus(&cal, None, &PropfindRequest::AllProp);
        assert!(!xml.contains("evt-1.ics"));
    }

    #[test]
    fn calendar_collection_depth_1_lists_event_children() {
        let cal = CalendarEntry {
            href: "/remote.php/dav/calendars/dev/personal/".to_string(),
            display_name: "Personal",
            color: None,
            ctag: "1",
        };
        let children = vec![(
            "/remote.php/dav/calendars/dev/personal/evt-1.ics".to_string(),
            "\"abc\"".to_string(),
        )];
        let xml = calendar_collection_multistatus(&cal, Some(&children), &PropfindRequest::AllProp);
        assert!(xml.contains("evt-1.ics</d:href>"));
        assert!(xml.contains("<d:getetag>&quot;abc&quot;</d:getetag>"));
    }
}
