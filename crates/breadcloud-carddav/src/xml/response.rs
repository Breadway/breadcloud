//! Building CardDAV `multistatus` XML.
//!
//! Same "own small property set per resource kind, not the file tree's
//! `ResourceEntry`/`build_multistatus`" decision
//! `breadcloud_caldav::xml::response` documents, applied to addressbooks
//! and contacts instead of calendars and events. There is no CardDAV
//! equivalent of that module's `principal_multistatus`/
//! `current_user_principal_multistatus` here: this crate does not own the
//! principal resource or the bare DAV root (see `crate::path`'s doc
//! comment) -- `addressbook-home-set` is spliced onto
//! `breadcloud-caldav`'s own principal response by
//! `breadcloud-server::combined` instead.

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

const NAMESPACES: &str = "xmlns:d=\"DAV:\" xmlns:card=\"urn:ietf:params:xml:ns:carddav\" \
     xmlns:cs=\"http://calendarserver.org/ns/\"";

fn multistatus_open(buf: &mut String) {
    buf.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    buf.push_str(&format!("<d:multistatus {NAMESPACES}>\n"));
}

fn multistatus_close(buf: &mut String) {
    buf.push_str("</d:multistatus>\n");
}

/// One `<d:prop>` entry this response knows how to render -- identical
/// shape to `breadcloud_caldav::xml::response`'s private `Prop`.
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

/// One addressbook collection to render inside an addressbook-home-set
/// listing.
pub struct AddressbookEntry<'a> {
    pub href: String,
    pub display_name: &'a str,
    pub ctag: &'a str,
}

/// PROPFIND response for `/remote.php/dav/addressbooks/<user>/` (Depth 0:
/// just the home-set collection; Depth 1: also every addressbook under
/// it).
pub fn addressbook_home_multistatus(
    home_href: &str,
    addressbooks: &[AddressbookEntry],
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
            value: "Addressbooks".to_string(),
        },
    ];
    render_response(&mut buf, home_href, &home_props, mode);

    if include_children {
        for book in addressbooks {
            render_addressbook_collection(&mut buf, book, mode);
        }
    }

    multistatus_close(&mut buf);
    buf
}

/// PROPFIND response for a single addressbook collection directly: Depth
/// 0 is just the collection's own properties (`children = None`); Depth 1
/// also lists each contact resource directly under it (`children =
/// Some(&[(href, etag)])`) -- same "answer both Depth 1 PROPFIND
/// enumeration and REPORT enumeration" stance
/// `breadcloud_caldav::xml::response::calendar_collection_multistatus`
/// documents, since vdirsyncer's live-verified behavior for CalDAV
/// (Depth 1 PROPFIND as the actual enumeration mechanism, not just
/// REPORT) is exactly the same code path its generic vcard/icalendar sync
/// engine uses for CardDAV too -- see the workspace README.
pub fn addressbook_collection_multistatus(
    book: &AddressbookEntry,
    children: Option<&[(String, String)]>,
    mode: &PropfindRequest,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    render_addressbook_collection(&mut buf, book, mode);
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

fn render_addressbook_collection(
    buf: &mut String,
    book: &AddressbookEntry,
    mode: &PropfindRequest,
) {
    let props = vec![
        Prop {
            tag: "d:resourcetype",
            value: "<d:collection/><card:addressbook/>".to_string(),
        },
        Prop {
            tag: "d:displayname",
            value: escape(book.display_name),
        },
        Prop {
            tag: "cs:getctag",
            value: escape(book.ctag),
        },
        Prop {
            tag: "d:sync-token",
            value: escape(&breadcloud_storage::AddressbookStore::format_sync_token(
                book.ctag,
            )),
        },
        Prop {
            tag: "card:supported-address-data",
            value: "<card:address-data-type content-type=\"text/vcard\" version=\"3.0\"/>\
                     <card:address-data-type content-type=\"text/vcard\" version=\"4.0\"/>"
                .to_string(),
        },
    ];
    render_response(buf, &book.href, &props, mode);
}

/// One contact to render in an `addressbook-query`/`addressbook-multiget`
/// REPORT response.
pub struct ContactEntry<'a> {
    pub href: String,
    pub etag: &'a str,
    pub vcf: Option<&'a [u8]>,
}

/// REPORT response for `addressbook-query`/`addressbook-multiget`: one
/// `<d:response>` per matched/fetched contact, each 200 OK with
/// `getetag`/`address-data` as requested.
pub fn addressbook_object_multistatus(entries: &[ContactEntry], props: &PropSet) -> String {
    addressbook_object_multistatus_with_missing(entries, &[], props)
}

/// REPORT response for `addressbook-multiget`, including a 404 response
/// for each requested href that could not be resolved.
pub fn addressbook_object_multistatus_with_missing(
    entries: &[ContactEntry],
    missing_hrefs: &[String],
    props: &PropSet,
) -> String {
    let mut buf = String::new();
    multistatus_open(&mut buf);
    for entry in entries {
        let mut prop_list = Vec::new();
        if props.getetag || !props.address_data {
            prop_list.push(Prop {
                tag: "d:getetag",
                value: escape(entry.etag),
            });
        }
        if props.address_data {
            if let Some(vcf) = entry.vcf {
                prop_list.push(Prop {
                    tag: "card:address-data",
                    value: escape(&String::from_utf8_lossy(vcf)),
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

/// One entry in a `sync-collection` REPORT's diff -- identical shape to
/// `breadcloud_caldav::xml::response::SyncEntry`.
pub struct SyncEntry<'a> {
    pub href: String,
    pub deleted: bool,
    pub etag: Option<&'a str>,
    pub vcf: Option<&'a [u8]>,
}

/// REPORT response for `sync-collection` (RFC 6578): a 200 OK
/// `<d:response>` per changed resource, a 404 `<d:response>` per removed
/// one, and the new `<d:sync-token>`.
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
        if props.address_data {
            if let Some(vcf) = entry.vcf {
                prop_list.push(Prop {
                    tag: "card:address-data",
                    value: escape(&String::from_utf8_lossy(vcf)),
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
    fn addressbook_home_lists_children_at_depth_1() {
        let books = vec![AddressbookEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/".to_string(),
            display_name: "Contacts",
            ctag: "3",
        }];
        let xml = addressbook_home_multistatus(
            "/remote.php/dav/addressbooks/dev/",
            &books,
            true,
            &PropfindRequest::AllProp,
        );
        assert!(xml.contains("<card:addressbook/>"));
        assert!(xml.contains("<cs:getctag>3</cs:getctag>"));
        assert!(xml.contains("contacts/</d:href>"));
    }

    #[test]
    fn addressbook_home_omits_children_at_depth_0() {
        let books = vec![AddressbookEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/".to_string(),
            display_name: "Contacts",
            ctag: "0",
        }];
        let xml = addressbook_home_multistatus(
            "/remote.php/dav/addressbooks/dev/",
            &books,
            false,
            &PropfindRequest::AllProp,
        );
        assert!(!xml.contains("contacts/</d:href>"));
    }

    #[test]
    fn addressbook_object_response_includes_address_data_when_requested() {
        let entries = vec![ContactEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/uid1.vcf".to_string(),
            etag: "\"abc\"",
            vcf: Some(b"BEGIN:VCARD\r\nEND:VCARD\r\n"),
        }];
        let props = PropSet {
            getetag: true,
            address_data: true,
        };
        let xml = addressbook_object_multistatus(&entries, &props);
        assert!(xml.contains("<card:address-data>BEGIN:VCARD"));
        assert!(xml.contains("<d:getetag>&quot;abc&quot;</d:getetag>"));
    }

    #[test]
    fn sync_collection_response_marks_deletions_404_and_includes_token() {
        let entries = vec![
            SyncEntry {
                href: "/remote.php/dav/addressbooks/dev/contacts/live.vcf".to_string(),
                deleted: false,
                etag: Some("\"etag1\""),
                vcf: None,
            },
            SyncEntry {
                href: "/remote.php/dav/addressbooks/dev/contacts/gone.vcf".to_string(),
                deleted: true,
                etag: None,
                vcf: None,
            },
        ];
        let props = PropSet {
            getetag: true,
            address_data: false,
        };
        let xml = sync_collection_multistatus(&entries, &props, "breadcloud-sync:9");
        assert!(xml.contains("live.vcf</d:href>"));
        assert!(xml.contains("gone.vcf</d:href>\n    <d:status>HTTP/1.1 404 Not Found</d:status>"));
        assert!(xml.contains("<d:sync-token>breadcloud-sync:9</d:sync-token>"));
    }

    #[test]
    fn propname_mode_omits_values() {
        let books = vec![AddressbookEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/".to_string(),
            display_name: "Contacts",
            ctag: "0",
        }];
        let xml = addressbook_home_multistatus(
            "/remote.php/dav/addressbooks/dev/",
            &books,
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
    fn addressbook_collection_depth_0_has_no_children() {
        let book = AddressbookEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/".to_string(),
            display_name: "Contacts",
            ctag: "1",
        };
        let xml = addressbook_collection_multistatus(&book, None, &PropfindRequest::AllProp);
        assert!(!xml.contains("ctc-1.vcf"));
    }

    #[test]
    fn addressbook_collection_depth_1_lists_contact_children() {
        let book = AddressbookEntry {
            href: "/remote.php/dav/addressbooks/dev/contacts/".to_string(),
            display_name: "Contacts",
            ctag: "1",
        };
        let children = vec![(
            "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf".to_string(),
            "\"abc\"".to_string(),
        )];
        let xml =
            addressbook_collection_multistatus(&book, Some(&children), &PropfindRequest::AllProp);
        assert!(xml.contains("ctc-1.vcf</d:href>"));
        assert!(xml.contains("<d:getetag>&quot;abc&quot;</d:getetag>"));
    }
}
