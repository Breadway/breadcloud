use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

/// A property name exactly as identified by its expanded XML name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedProperty {
    pub namespace: Option<String>,
    pub local_name: String,
}

impl RequestedProperty {
    fn from_resolved(namespace: ResolveResult<'_>, local_name: String) -> Option<Self> {
        let namespace = match namespace {
            ResolveResult::Bound(namespace) => {
                // NsReader returns raw namespace attribute bytes, including XML
                // references. Normalize literal attribute whitespace before
                // decoding references, which must retain their referenced chars.
                let raw = std::str::from_utf8(namespace.0).ok()?;
                let normalized = raw.replace("\r\n", " ").replace(['\r', '\n', '\t'], " ");
                let decoded = quick_xml::escape::unescape(&normalized).ok()?;
                if !decoded.chars().all(|c| {
                    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
                }) {
                    return None;
                }
                Some(decoded.into_owned())
            }
            ResolveResult::Unbound | ResolveResult::Unknown(_) => None,
        };
        Some(Self {
            namespace,
            local_name,
        })
    }
}

/// What a PROPFIND request body asked for.
///
/// Real clients (Nextcloud's mirall, iOS's Files app) almost always send
/// an explicit `<prop>` list; an empty body or `<allprop/>` both mean "give
/// me the standard set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropfindRequest {
    AllProp,
    PropName,
    Prop(Vec<RequestedProperty>),
}

/// Parse a PROPFIND request body, retaining each requested property's
/// namespace URI and local name. Prefixes are resolved because clients may
/// bind different prefixes to the same namespace. An empty or unparseable
/// body is treated as
/// `allprop`, which is the safe default (RFC 4918 14.1: no body means the
/// same as an `allprop` request in practice for the clients we target).
pub fn parse_propfind_body(body: &[u8]) -> PropfindRequest {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return PropfindRequest::AllProp;
    }

    let mut reader = NsReader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut names: Vec<RequestedProperty> = Vec::new();
    let mut in_prop = false;
    let mut prop_child_depth: u32 = 0;
    let mut saw_allprop = false;
    let mut saw_propname = false;

    while let Ok((namespace, event)) = reader.read_resolved_event() {
        match event {
            Event::Start(e) => {
                let local = local_name(e.local_name().as_ref());
                if in_prop {
                    prop_child_depth += 1;
                    if prop_child_depth == 1 {
                        let Some(property) = RequestedProperty::from_resolved(namespace, local)
                        else {
                            return PropfindRequest::AllProp;
                        };
                        names.push(property);
                    }
                } else {
                    match local.as_str() {
                        "allprop" => saw_allprop = true,
                        "propname" => saw_propname = true,
                        "prop" => in_prop = true,
                        _ => {}
                    }
                }
            }
            Event::Empty(e) => {
                let local = local_name(e.local_name().as_ref());
                if in_prop {
                    if prop_child_depth == 0 {
                        let Some(property) = RequestedProperty::from_resolved(namespace, local)
                        else {
                            return PropfindRequest::AllProp;
                        };
                        names.push(property);
                    }
                } else {
                    match local.as_str() {
                        "allprop" => saw_allprop = true,
                        "propname" => saw_propname = true,
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                let local = local_name(e.local_name().as_ref());
                if in_prop {
                    if local == "prop" && prop_child_depth == 0 {
                        in_prop = false;
                    } else {
                        prop_child_depth = prop_child_depth.saturating_sub(1);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if saw_allprop {
        PropfindRequest::AllProp
    } else if saw_propname {
        PropfindRequest::PropName
    } else if !names.is_empty() {
        PropfindRequest::Prop(names)
    } else {
        PropfindRequest::AllProp
    }
}

/// Local (non-namespace-prefixed) part of a possibly-qualified XML tag
/// name, e.g. `d:getetag` -> `getetag`.
fn local_name(qname: &[u8]) -> String {
    let s = std::str::from_utf8(qname).unwrap_or("");
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_is_allprop() {
        assert_eq!(parse_propfind_body(b""), PropfindRequest::AllProp);
    }

    #[test]
    fn explicit_allprop() {
        let body = br#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:allprop/></d:propfind>"#;
        assert_eq!(parse_propfind_body(body), PropfindRequest::AllProp);
    }

    #[test]
    fn propname_request() {
        let body = br#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:propname/></d:propfind>"#;
        assert_eq!(parse_propfind_body(body), PropfindRequest::PropName);
    }

    #[test]
    fn explicit_prop_list_like_mirall_sends() {
        let body = br#"<?xml version="1.0"?>
<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">
  <d:prop>
    <d:getetag/>
    <d:getlastmodified/>
    <d:getcontentlength/>
    <d:resourcetype/>
    <oc:fileid/>
  </d:prop>
</d:propfind>"#;
        let parsed = parse_propfind_body(body);
        match parsed {
            PropfindRequest::Prop(names) => {
                assert_eq!(names.len(), 5);
                assert_eq!(names[0].namespace.as_deref(), Some("DAV:"));
                assert_eq!(names[0].local_name, "getetag");
                assert_eq!(names[1].local_name, "getlastmodified");
                assert_eq!(names[2].local_name, "getcontentlength");
                assert_eq!(names[3].local_name, "resourcetype");
                assert_eq!(
                    names[4].namespace.as_deref(),
                    Some("http://owncloud.org/ns")
                );
                assert_eq!(names[4].local_name, "fileid");
            }
            other => panic!("expected Prop, got {other:?}"),
        }
    }

    #[test]
    fn requested_property_keeps_default_namespace() {
        let body = br#"<propfind xmlns="DAV:" xmlns:nc="http://nextcloud.org/ns"><prop><nc:has-preview/><getetag/></prop></propfind>"#;
        let PropfindRequest::Prop(names) = parse_propfind_body(body) else {
            panic!("expected Prop request");
        };
        assert_eq!(
            names[0].namespace.as_deref(),
            Some("http://nextcloud.org/ns")
        );
        assert_eq!(names[0].local_name, "has-preview");
        assert_eq!(names[1].namespace.as_deref(), Some("DAV:"));
        assert_eq!(names[1].local_name, "getetag");
    }

    #[test]
    fn namespace_attribute_normalizes_literal_whitespace_before_references() {
        let body = b"<d:propfind xmlns:d=\"DAV:\" xmlns:x=\"urn:a\r\nb\tc&#9;d\"><d:prop><x:custom/></d:prop></d:propfind>";
        let PropfindRequest::Prop(names) = parse_propfind_body(body) else {
            panic!("expected Prop request");
        };
        assert_eq!(names[0].namespace.as_deref(), Some("urn:a b c\td"));
    }

    #[test]
    fn malformed_namespace_reference_uses_safe_fallback() {
        for raw in ["urn:&undefined;", "urn:&#xZZ;", "urn:&#0;", "urn:&#1;"] {
            let body = format!(
                "<d:propfind xmlns:d=\"DAV:\" xmlns:x=\"{raw}\"><d:prop><x:custom/></d:prop></d:propfind>"
            );
            assert_eq!(
                parse_propfind_body(body.as_bytes()),
                PropfindRequest::AllProp
            );
        }
    }
}
