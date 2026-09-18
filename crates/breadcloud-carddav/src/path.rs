//! Parsing a decoded CardDAV request path into the resource it addresses.
//!
//! Same fixed-shape splitting `breadcloud_caldav::path::CalPath` does for
//! calendar URLs, applied to the addressbook tree instead
//! (`/remote.php/dav/addressbooks/<user>/<addressbook id>/<object>`).
//! Unlike `CalPath`, there is no `Root`/`Principal` variant here: this
//! crate does not own the bare DAV root or the principal resource (both
//! stay `breadcloud-caldav`'s, since there is exactly one principal per
//! user regardless of how many DAV extensions it has home-sets for) --
//! see `breadcloud-server::combined` for how `addressbook-home-set` gets
//! onto that shared principal response.

/// A parsed CardDAV request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardPath {
    /// `/remote.php/dav/addressbooks/<user>/` -- the addressbook-home-set
    /// collection itself.
    AddressbooksHome(String),
    /// `/remote.php/dav/addressbooks/<user>/<addressbook id>/`.
    Addressbook(String, String),
    /// `/remote.php/dav/addressbooks/<user>/<addressbook id>/<object>` --
    /// a single contact resource, `<object>` kept exactly as given
    /// (typically `<uid>.vcf`; see `handlers::contact`).
    Contact(String, String, String),
}

impl CardPath {
    pub fn parse(decoded: &str) -> Option<CardPath> {
        let rest = decoded.strip_prefix("/remote.php/dav/addressbooks/")?;
        let trimmed = rest.trim_end_matches('/');
        if trimmed.is_empty() {
            return None;
        }
        let segs: Vec<&str> = trimmed.split('/').collect();
        match segs.as_slice() {
            [user] if !user.is_empty() => Some(CardPath::AddressbooksHome(user.to_string())),
            [user, book] if !user.is_empty() && !book.is_empty() => {
                Some(CardPath::Addressbook(user.to_string(), book.to_string()))
            }
            [user, book, obj] if !user.is_empty() && !book.is_empty() && !obj.is_empty() => Some(
                CardPath::Contact(user.to_string(), book.to_string(), obj.to_string()),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addressbooks_home() {
        assert_eq!(
            CardPath::parse("/remote.php/dav/addressbooks/dev/"),
            Some(CardPath::AddressbooksHome("dev".to_string()))
        );
    }

    #[test]
    fn parses_addressbook_collection() {
        assert_eq!(
            CardPath::parse("/remote.php/dav/addressbooks/dev/contacts/"),
            Some(CardPath::Addressbook(
                "dev".to_string(),
                "contacts".to_string()
            ))
        );
        assert_eq!(
            CardPath::parse("/remote.php/dav/addressbooks/dev/contacts"),
            Some(CardPath::Addressbook(
                "dev".to_string(),
                "contacts".to_string()
            ))
        );
    }

    #[test]
    fn parses_contact_object() {
        assert_eq!(
            CardPath::parse("/remote.php/dav/addressbooks/dev/contacts/abc-123.vcf"),
            Some(CardPath::Contact(
                "dev".to_string(),
                "contacts".to_string(),
                "abc-123.vcf".to_string()
            ))
        );
    }

    #[test]
    fn rejects_too_many_segments() {
        assert_eq!(
            CardPath::parse("/remote.php/dav/addressbooks/dev/contacts/abc.vcf/extra"),
            None
        );
    }

    #[test]
    fn unrelated_paths_are_none() {
        assert_eq!(
            CardPath::parse("/remote.php/dav/calendars/dev/personal/"),
            None
        );
        assert_eq!(CardPath::parse("/remote.php/dav/principals/dev/"), None);
        assert_eq!(CardPath::parse("/status.php"), None);
    }
}
