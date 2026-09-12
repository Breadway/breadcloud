use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// What a PROPFIND request body asked for.
///
/// Real clients (Nextcloud's mirall, iOS's Files app) almost always send
/// an explicit `<prop>` list; an empty body or `<allprop/>` both mean "give
/// me the standard set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropfindRequest {
    AllProp,
    PropName,
    Prop(Vec<String>),
}

/// Parse a PROPFIND request body. Namespace-agnostic: property names are
/// matched by local name only, since every property this server
/// understands lives in the `DAV:` namespace and clients vary in which
/// prefix they bind to it. An empty or unparseable body is treated as
/// `allprop`, which is the safe default (RFC 4918 14.1: no body means the
/// same as an `allprop` request in practice for the clients we target).
pub fn parse_propfind_body(body: &[u8]) -> PropfindRequest {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return PropfindRequest::AllProp;
    }

    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);

    let mut names: Vec<String> = Vec::new();
    let mut in_prop = false;
    let mut prop_child_depth: u32 = 0;
    let mut saw_allprop = false;
    let mut saw_propname = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let local = local_name(e.name().as_ref());
                if in_prop {
                    prop_child_depth += 1;
                    if prop_child_depth == 1 {
                        names.push(local);
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
            Ok(Event::Empty(e)) => {
                let local = local_name(e.name().as_ref());
                if in_prop {
                    if prop_child_depth == 0 {
                        names.push(local);
                    }
                } else {
                    match local.as_str() {
                        "allprop" => saw_allprop = true,
                        "propname" => saw_propname = true,
                        _ => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let local = local_name(e.name().as_ref());
                if in_prop {
                    if local == "prop" && prop_child_depth == 0 {
                        in_prop = false;
                    } else {
                        prop_child_depth = prop_child_depth.saturating_sub(1);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
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
                assert_eq!(
                    names,
                    vec![
                        "getetag",
                        "getlastmodified",
                        "getcontentlength",
                        "resourcetype",
                        "fileid"
                    ]
                );
            }
            other => panic!("expected Prop, got {other:?}"),
        }
    }
}
