//! Parsing a decoded Notes OCS API request path into the route it
//! addresses.
//!
//! Real Nextcloud Notes clients (confirmed against the app's own
//! `appinfo/routes.php`: `nextcloud/notes` on GitHub) call
//! `/index.php/apps/notes/api/v1/...` -- the app id `notes`, API version
//! segment `v1` (not `v1.0`, not `1`), everything after that a plain
//! path, no OCS envelope. This module accepts that exact prefix, and also
//! the same path with `/index.php` dropped, for a server configured with
//! Nextcloud's "pretty URLs" rewrite (both forms route to the same app in
//! real Nextcloud, and nothing here needs to tell them apart).

use breadcloud_storage::DavPath;

const PREFIXES: [&str; 2] = ["/index.php/apps/notes/api/v1", "/apps/notes/api/v1"];

/// Strip whichever of [`PREFIXES`] `decoded_path` starts with, if any.
fn strip_api_prefix(decoded_path: &str) -> Option<&str> {
    PREFIXES
        .iter()
        .find_map(|prefix| decoded_path.strip_prefix(prefix))
}

pub fn owns_path(decoded_path: &str) -> bool {
    strip_api_prefix(decoded_path).is_some()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesRoute {
    /// `/notes`: `GET` lists, `POST` creates.
    Notes,
    /// `/notes/{id}`: `GET` fetches, `PUT` updates, `DELETE` removes.
    Note(String),
    /// `/settings`: `GET`/`PUT`.
    Settings,
    /// `/attachment/{noteid}`: `GET`/`POST`/`DELETE`.
    Attachment(String),
}

impl NotesRoute {
    pub fn parse(decoded_path: &str) -> Option<NotesRoute> {
        let rest = strip_api_prefix(decoded_path)?;

        if rest == "/notes" {
            return Some(NotesRoute::Notes);
        }
        if let Some(id) = rest.strip_prefix("/notes/") {
            if id.is_empty() || id.contains('/') {
                return None;
            }
            return Some(NotesRoute::Note(id.to_string()));
        }
        if rest == "/settings" {
            return Some(NotesRoute::Settings);
        }
        if let Some(id) = rest.strip_prefix("/attachment/") {
            if id.is_empty() || id.contains('/') {
                return None;
            }
            return Some(NotesRoute::Attachment(id.to_string()));
        }
        None
    }
}

/// The account-relative `DavPath` of the top-level `Notes/` folder every
/// note lives under (or a subfolder of, for a categorized note) -- the
/// same folder a plain WebDAV/desktop-sync client sees and edits, per the
/// issue's own framing ("Nextcloud Notes are just markdown files under a
/// Notes/ folder").
pub fn notes_root() -> DavPath {
    DavPath::parse("/Notes").expect("\"/Notes\" is always a valid DavPath")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owns_the_documented_prefix() {
        assert!(owns_path("/index.php/apps/notes/api/v1/notes"));
        assert!(owns_path("/index.php/apps/notes/api/v1/settings"));
        assert!(!owns_path("/remote.php/dav/files/dev/Notes/a.md"));
        assert!(!owns_path("/index.php/apps/notes/api/v2/notes"));
    }

    #[test]
    fn owns_the_pretty_url_form_too() {
        assert!(owns_path("/apps/notes/api/v1/notes"));
    }

    #[test]
    fn parses_notes_collection_and_item() {
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/notes"),
            Some(NotesRoute::Notes)
        );
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/notes/42"),
            Some(NotesRoute::Note("42".to_string()))
        );
    }

    #[test]
    fn parses_settings_and_attachment() {
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/settings"),
            Some(NotesRoute::Settings)
        );
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/attachment/42"),
            Some(NotesRoute::Attachment("42".to_string()))
        );
    }

    #[test]
    fn rejects_nested_or_empty_id_segments() {
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/notes/"),
            None
        );
        assert_eq!(
            NotesRoute::parse("/index.php/apps/notes/api/v1/notes/42/extra"),
            None
        );
    }

    #[test]
    fn unrelated_paths_are_none() {
        assert_eq!(NotesRoute::parse("/status.php"), None);
        assert_eq!(NotesRoute::parse("/ui/files"), None);
    }
}
