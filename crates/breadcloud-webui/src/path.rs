//! Parsing a decoded `/ui/...` request path into the page or action it
//! addresses. Same shallow, fixed-shape parsing `breadcloud_caldav::CalPath`
//! does for its own URL space -- see that module's doc comment.
//!
//! Deliberately no `<user>` segment anywhere in this URL space (unlike
//! `/remote.php/dav/files/<user>/...` etc): every `/ui/*` route always
//! operates against whichever account `breadcloud-server::combined`'s
//! `auth_gate` already resolved for this request, never a URL-supplied
//! one, so there is no "does the URL's user match the authenticated
//! user" check to get right or forget here at all (CLOUD-11's whole
//! `url_scoped_user` mechanism has nothing to validate in this URL
//! space).

use breadcloud_storage::DavPath;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRoute {
    /// `/ui` or `/ui/`: the landing page.
    Home,
    /// `/ui/files` or `/ui/files/<path>`: browse a directory (or, if
    /// `<path>` names a file, redirect to its download link).
    Files(DavPath),
    /// `/ui/download/<path>`: stream a file's content.
    Download(DavPath),
    /// `/ui/calendar`: the month view.
    Calendar,
    /// `/ui/style.css`: the shared stylesheet (CLOUD-24) every page links
    /// to.
    Style,
    /// `/ui/upload` (POST): a multipart file upload.
    Upload,
    /// `/ui/mkdir` (POST): create a folder.
    Mkdir,
    /// `/ui/delete` (POST): delete a file or folder.
    Delete,
}

impl UiRoute {
    pub fn parse(decoded_path: &str) -> Option<UiRoute> {
        let rest = decoded_path.strip_prefix("/ui")?;
        if rest.is_empty() || rest == "/" {
            return Some(UiRoute::Home);
        }

        if rest == "/files" || rest == "/files/" {
            return Some(UiRoute::Files(DavPath::root()));
        }
        if let Some(sub) = rest.strip_prefix("/files/") {
            return DavPath::parse(sub).ok().map(UiRoute::Files);
        }

        // Exact "/download" or "/download/" (nothing named) is not a
        // route at all -- checked before the prefix strip below so it
        // does not fall through to `DavPath::parse("")`, which would
        // otherwise resolve to the storage root.
        if rest == "/download" || rest == "/download/" {
            return None;
        }
        if let Some(sub) = rest.strip_prefix("/download/") {
            return DavPath::parse(sub).ok().map(UiRoute::Download);
        }

        if rest == "/calendar" || rest == "/calendar/" {
            return Some(UiRoute::Calendar);
        }
        if rest == "/style.css" {
            return Some(UiRoute::Style);
        }
        if rest == "/upload" {
            return Some(UiRoute::Upload);
        }
        if rest == "/mkdir" {
            return Some(UiRoute::Mkdir);
        }
        if rest == "/delete" {
            return Some(UiRoute::Delete);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_home() {
        assert_eq!(UiRoute::parse("/ui"), Some(UiRoute::Home));
        assert_eq!(UiRoute::parse("/ui/"), Some(UiRoute::Home));
    }

    #[test]
    fn parses_files_root() {
        assert_eq!(
            UiRoute::parse("/ui/files"),
            Some(UiRoute::Files(DavPath::root()))
        );
        assert_eq!(
            UiRoute::parse("/ui/files/"),
            Some(UiRoute::Files(DavPath::root()))
        );
    }

    #[test]
    fn parses_files_subpath() {
        assert_eq!(
            UiRoute::parse("/ui/files/docs/report.pdf"),
            Some(UiRoute::Files(DavPath::parse("/docs/report.pdf").unwrap()))
        );
    }

    #[test]
    fn parses_download() {
        assert_eq!(
            UiRoute::parse("/ui/download/docs/report.pdf"),
            Some(UiRoute::Download(
                DavPath::parse("/docs/report.pdf").unwrap()
            ))
        );
    }

    #[test]
    fn bare_download_is_not_a_route() {
        assert_eq!(UiRoute::parse("/ui/download"), None);
        assert_eq!(UiRoute::parse("/ui/download/"), None);
    }

    #[test]
    fn parses_calendar_and_actions() {
        assert_eq!(UiRoute::parse("/ui/calendar"), Some(UiRoute::Calendar));
        assert_eq!(UiRoute::parse("/ui/upload"), Some(UiRoute::Upload));
        assert_eq!(UiRoute::parse("/ui/mkdir"), Some(UiRoute::Mkdir));
        assert_eq!(UiRoute::parse("/ui/delete"), Some(UiRoute::Delete));
    }

    #[test]
    fn parses_stylesheet() {
        assert_eq!(UiRoute::parse("/ui/style.css"), Some(UiRoute::Style));
    }

    #[test]
    fn unrelated_paths_are_none() {
        assert_eq!(UiRoute::parse("/remote.php/dav/files/dev/a.txt"), None);
        assert_eq!(UiRoute::parse("/status.php"), None);
        assert_eq!(UiRoute::parse("/uiwithnoslash"), None);
    }

    #[test]
    fn rejects_traversal_in_files_and_download() {
        assert_eq!(UiRoute::parse("/ui/files/../../etc/passwd"), None);
        assert_eq!(UiRoute::parse("/ui/download/../../etc/passwd"), None);
    }
}
