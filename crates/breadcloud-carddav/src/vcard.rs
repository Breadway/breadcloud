//! Minimal vCard (RFC 6350, and the RFC 2426/vCard 3 shape most real
//! exports still use) reading, just deep enough for what the CardDAV
//! surface needs: pulling a `UID` out of a PUT body, and a best-effort
//! property text-match for `addressbook-query` filtering (RFC 6352 8.6.4).
//! Deliberately not a general vCard parser/writer, the same scope
//! decision `breadcloud_caldav::ical` makes for iCalendar and for the
//! same reason: contacts are stored and served as opaque bytes (see
//! `breadcloud_storage::AddressbookStore`), this module only ever reads
//! just enough of that byte content to answer "what UID is this", "is
//! this a group vCard", and "does this property's value contain this
//! text".
//!
//! **Contact groups are ordinary vCards, not a protocol-level concept.**
//! The real data this issue targets (76 contacts split across
//! "Friends"/"ICE"/"starred"/"Family team" groups) is represented the
//! standard CardDAV way, per RFC 6350 section 6.1.4 and 6.6.5: a group is
//! a vCard like any other, with `KIND:group` and one `MEMBER:urn:uuid:<uid>`
//! line per member, referencing each member contact's own `UID`. Both
//! DAVx5 and iOS Contacts read and write groups in exactly this shape, so
//! there is nothing else for this server to implement -- `PUT`/`GET`/
//! `DELETE`/`REPORT` treat a group vCard exactly like a person vCard,
//! since the addressbook storage layer has no notion of "kind" at all
//! (see `breadcloud_storage::addressbook`'s module doc comment). This
//! module's `extract_kind` exists only so `addressbook-query`'s optional
//! `CARD:group` component filter (if a client ever sends one) can
//! distinguish the two without this server needing a second resource
//! type.

/// Unfold RFC 6350 folded lines -- identical rule and identical
/// implementation to `breadcloud_caldav::ical::unfold` (a CRLF or LF
/// immediately followed by a single space or tab continues the previous
/// line). Duplicated rather than shared across crates: see the workspace
/// README's note on why `breadcloud-carddav` does not depend on
/// `breadcloud-caldav` for this kind of small, format-specific helper.
fn unfold(vcf: &str) -> String {
    let normalized = vcf.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    for line in normalized.split('\n') {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.push_str(&line[1..]);
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

fn split_line(line: &str) -> Option<(&str, &str)> {
    line.split_once(':')
}

fn property_name(name_and_params: &str) -> &str {
    name_and_params
        .split_once(';')
        .map(|(name, _)| name)
        .unwrap_or(name_and_params)
}

/// Extract the `UID` of a vCard. `None` if there is no `UID` line at all
/// -- RFC 6350 does not strictly require one, but every real CardDAV
/// client (DAVx5, iOS) always sends one, and this server requires it on
/// PUT the same way `breadcloud_caldav`'s event PUT requires a calendar
/// object's `UID` (see `handlers::contact::put`): a contact resource
/// needs a stable identity `sync-collection` and `addressbook-multiget`
/// can address, and the UID is that identity.
pub fn extract_uid(vcf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(vcf);
    let unfolded = unfold(&text);
    for line in unfolded.lines() {
        if let Some((name, value)) = split_line(line) {
            if property_name(name).eq_ignore_ascii_case("UID") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Extract the `KIND` property's value (e.g. `"group"`, `"individual"`),
/// lowercased. `None` if absent -- vCard 3 has no `KIND` property at all
/// (every vCard 3 export is implicitly an individual), and `None` is
/// treated as "not a group" by `has_kind`.
pub fn extract_kind(vcf: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(vcf);
    let unfolded = unfold(&text);
    for line in unfolded.lines() {
        if let Some((name, value)) = split_line(line) {
            if property_name(name).eq_ignore_ascii_case("KIND") {
                return Some(value.trim().to_ascii_lowercase());
            }
        }
    }
    None
}

/// Whether this vCard's `KIND` equals `kind` (case-insensitive), e.g.
/// `has_kind(vcf, "group")`.
pub fn has_kind(vcf: &[u8], kind: &str) -> bool {
    extract_kind(vcf).as_deref() == Some(&kind.to_ascii_lowercase())
}

/// RFC 6352 8.6.4 `prop-filter`/`text-match`: whether the named
/// property's value contains `needle` (case-insensitive substring match).
/// If the vCard has more than one line for `prop_name` (e.g. multiple
/// `EMAIL` lines), any matching line counts. `false` if the property is
/// absent entirely -- matching RFC 6352's own "no value to match against"
/// semantics for a plain (non-negated) `text-match`.
pub fn text_match(vcf: &[u8], prop_name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let text = String::from_utf8_lossy(vcf);
    let unfolded = unfold(&text);
    let needle_lower = needle.to_ascii_lowercase();
    unfolded.lines().any(|line| {
        split_line(line)
            .map(|(name, value)| {
                property_name(name).eq_ignore_ascii_case(prop_name)
                    && value.to_ascii_lowercase().contains(&needle_lower)
            })
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(uid: &str, fn_value: &str) -> Vec<u8> {
        format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{fn_value}\r\nEND:VCARD\r\n")
            .into_bytes()
    }

    #[test]
    fn extracts_uid() {
        let vcf = person("abc-123", "Ada Lovelace");
        assert_eq!(extract_uid(&vcf).as_deref(), Some("abc-123"));
    }

    #[test]
    fn missing_uid_is_none() {
        let vcf = b"BEGIN:VCARD\r\nVERSION:4.0\r\nFN:No Uid\r\nEND:VCARD\r\n";
        assert_eq!(extract_uid(vcf), None);
    }

    #[test]
    fn unfolds_wrapped_lines() {
        let vcf = b"BEGIN:VCARD\r\nUID:abc-\r\n 123\r\nEND:VCARD\r\n";
        assert_eq!(extract_uid(vcf).as_deref(), Some("abc-123"));
    }

    #[test]
    fn detects_group_kind() {
        let vcf =
            b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:g1\r\nFN:Friends\r\nKIND:group\r\nMEMBER:urn:uuid:abc\r\nEND:VCARD\r\n";
        assert!(has_kind(vcf, "group"));
        assert!(!has_kind(vcf, "individual"));
    }

    #[test]
    fn no_kind_is_not_a_group() {
        let vcf = person("abc-123", "Ada Lovelace");
        assert!(!has_kind(&vcf, "group"));
    }

    #[test]
    fn text_match_is_case_insensitive_substring() {
        let vcf = person("abc-123", "Ada Lovelace");
        assert!(text_match(&vcf, "FN", "lovelace"));
        assert!(text_match(&vcf, "FN", "ADA"));
        assert!(!text_match(&vcf, "FN", "Babbage"));
    }

    #[test]
    fn text_match_on_missing_property_is_false() {
        let vcf = person("abc-123", "Ada Lovelace");
        assert!(!text_match(&vcf, "EMAIL", "ada@example.com"));
    }

    #[test]
    fn empty_needle_always_matches() {
        let vcf = person("abc-123", "Ada Lovelace");
        assert!(text_match(&vcf, "FN", ""));
    }
}
