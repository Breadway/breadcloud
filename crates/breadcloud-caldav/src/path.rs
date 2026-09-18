//! Parsing a decoded CalDAV request path into the resource it addresses.
//!
//! Unlike `breadcloud_storage::DavPath`, this is not a general
//! traversal-safe path type sandboxed to a filesystem root: a CalDAV URL
//! has a fixed, shallow shape (`/remote.php/dav/calendars/<user>/<calendar
//! id>/<object>`), so this just splits it into that shape's named parts.
//! Traversal safety for the parts that do end up touching the filesystem
//! (the user and calendar id) is `breadcloud_storage::CalendarStore`'s
//! job, via the same `DavPath`-based segment validation the file tree
//! uses -- see that crate's `calendar` module.

/// A parsed CalDAV request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalPath {
    /// The bare DAV root (`/remote.php/dav` or `/remote.php/dav/`), used
    /// for `current-user-principal` discovery.
    Root,
    /// `/remote.php/dav/principals/<user>/`.
    Principal(String),
    /// `/remote.php/dav/calendars/<user>/` -- the calendar-home-set
    /// collection itself.
    CalendarsHome(String),
    /// `/remote.php/dav/calendars/<user>/<calendar id>/`.
    Calendar(String, String),
    /// `/remote.php/dav/calendars/<user>/<calendar id>/<object>` -- a
    /// single calendar object resource, `<object>` kept exactly as given
    /// (typically `<uid>.ics`, but this layer does not assume the `.ics`
    /// suffix or otherwise interpret it; see `handlers::event`).
    Event(String, String, String),
}

impl CalPath {
    pub fn parse(decoded: &str) -> Option<CalPath> {
        if decoded == "/remote.php/dav" || decoded == "/remote.php/dav/" {
            return Some(CalPath::Root);
        }

        if let Some(rest) = decoded.strip_prefix("/remote.php/dav/principals/") {
            let user = rest.trim_end_matches('/');
            if user.is_empty() || user.contains('/') {
                return None;
            }
            return Some(CalPath::Principal(user.to_string()));
        }

        if let Some(rest) = decoded.strip_prefix("/remote.php/dav/calendars/") {
            let trimmed = rest.trim_end_matches('/');
            if trimmed.is_empty() {
                return None;
            }
            let segs: Vec<&str> = trimmed.split('/').collect();
            return match segs.as_slice() {
                [user] if !user.is_empty() => Some(CalPath::CalendarsHome(user.to_string())),
                [user, cal] if !user.is_empty() && !cal.is_empty() => {
                    Some(CalPath::Calendar(user.to_string(), cal.to_string()))
                }
                [user, cal, obj] if !user.is_empty() && !cal.is_empty() && !obj.is_empty() => Some(
                    CalPath::Event(user.to_string(), cal.to_string(), obj.to_string()),
                ),
                _ => None,
            };
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_root() {
        assert_eq!(CalPath::parse("/remote.php/dav"), Some(CalPath::Root));
        assert_eq!(CalPath::parse("/remote.php/dav/"), Some(CalPath::Root));
    }

    #[test]
    fn parses_principal() {
        assert_eq!(
            CalPath::parse("/remote.php/dav/principals/dev/"),
            Some(CalPath::Principal("dev".to_string()))
        );
        assert_eq!(
            CalPath::parse("/remote.php/dav/principals/dev"),
            Some(CalPath::Principal("dev".to_string()))
        );
    }

    #[test]
    fn rejects_principal_with_extra_segments() {
        assert_eq!(CalPath::parse("/remote.php/dav/principals/dev/extra"), None);
    }

    #[test]
    fn parses_calendars_home() {
        assert_eq!(
            CalPath::parse("/remote.php/dav/calendars/dev/"),
            Some(CalPath::CalendarsHome("dev".to_string()))
        );
    }

    #[test]
    fn parses_calendar_collection() {
        assert_eq!(
            CalPath::parse("/remote.php/dav/calendars/dev/personal/"),
            Some(CalPath::Calendar("dev".to_string(), "personal".to_string()))
        );
        assert_eq!(
            CalPath::parse("/remote.php/dav/calendars/dev/personal"),
            Some(CalPath::Calendar("dev".to_string(), "personal".to_string()))
        );
    }

    #[test]
    fn parses_event_object() {
        assert_eq!(
            CalPath::parse("/remote.php/dav/calendars/dev/personal/abc-123.ics"),
            Some(CalPath::Event(
                "dev".to_string(),
                "personal".to_string(),
                "abc-123.ics".to_string()
            ))
        );
    }

    #[test]
    fn rejects_too_many_segments() {
        assert_eq!(
            CalPath::parse("/remote.php/dav/calendars/dev/personal/abc.ics/extra"),
            None
        );
    }

    #[test]
    fn unrelated_paths_are_none() {
        assert_eq!(CalPath::parse("/remote.php/dav/files/dev/a.txt"), None);
        assert_eq!(CalPath::parse("/status.php"), None);
    }
}
