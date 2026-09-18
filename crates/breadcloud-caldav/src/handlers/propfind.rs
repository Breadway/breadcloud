use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError};
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::{parse_depth, read_propfind_mode, xml_response, Depth};
use crate::state::AppState;
use crate::xml::response::{self, CalendarEntry};

/// PROPFIND on `/remote.php/dav/calendars/<user>/`, the calendar-home-set
/// collection itself: Depth 0 answers only the collection's own
/// properties, Depth 1 (the default here -- see `crate::handlers::Depth`)
/// also lists every calendar under it, which is how a client discovers
/// the personal/family/school calendars in the first place.
pub async fn calendar_home(state: &AppState, user: &str, req: Request) -> Response {
    let depth = parse_depth(req.headers());
    let mode = read_propfind_mode(req).await;

    let calendars = match state.calendars.list_calendars(user).await {
        Ok(c) => c,
        Err(err) => return storage_error_response(err),
    };
    let entries: Vec<CalendarEntry> = calendars
        .iter()
        .map(|c| CalendarEntry {
            href: format!("/remote.php/dav/calendars/{user}/{}/", c.id),
            display_name: &c.display_name,
            color: c.color.as_deref(),
            ctag: &c.ctag,
        })
        .collect();

    let home_href = format!("/remote.php/dav/calendars/{user}/");
    let xml = response::calendar_home_multistatus(&home_href, &entries, depth == Depth::One, &mode);
    xml_response(xml)
}

/// PROPFIND on a single calendar collection
/// (`/remote.php/dav/calendars/<user>/<calendar id>/`): Depth 0 answers
/// only the collection's own properties (`getctag`, `calendar-color`,
/// `supported-calendar-component-set`, and so on); Depth 1 also lists
/// every event resource directly under it (href + getetag only). A
/// client can fetch the events themselves either via this Depth 1
/// listing followed by individual GETs, via `REPORT
/// calendar-multiget`/`calendar-query`, or via `REPORT sync-collection`
/// (`crate::handlers::report`) -- real clients are not consistent about
/// which they prefer (vdirsyncer's CalDAV storage backend was confirmed,
/// live, to require this Depth 1 listing; see the workspace README), so
/// this server supports both rather than assuming REPORT alone is
/// enough.
pub async fn calendar_collection(
    state: &AppState,
    user: &str,
    calendar_id: &str,
    req: Request,
) -> Response {
    let depth = parse_depth(req.headers());
    let mode = read_propfind_mode(req).await;

    let info = match state.calendars.get_calendar(user, calendar_id).await {
        Ok(c) => c,
        Err(StorageError::NotFound) => {
            return (StatusCode::NOT_FOUND, "no such calendar").into_response()
        }
        Err(err) => return storage_error_response(err),
    };

    let entry = CalendarEntry {
        href: format!("/remote.php/dav/calendars/{user}/{calendar_id}/"),
        display_name: &info.display_name,
        color: info.color.as_deref(),
        ctag: &info.ctag,
    };

    let children = if depth == Depth::One {
        match list_event_children(state, user, calendar_id).await {
            Ok(c) => Some(c),
            Err(err) => return storage_error_response(err),
        }
    } else {
        None
    };

    xml_response(response::calendar_collection_multistatus(
        &entry,
        children.as_deref(),
        &mode,
    ))
}

async fn list_event_children(
    state: &AppState,
    user: &str,
    calendar_id: &str,
) -> Result<Vec<(String, String)>, StorageError> {
    let events = state.calendars.events_storage(user, calendar_id).await?;
    let listing = events.list_dir(&DavPath::root()).await?;
    let mut children = Vec::with_capacity(listing.len());
    for entry in listing {
        if entry.metadata.is_dir() {
            continue;
        }
        let etag = events.etag_for(&entry.path, &entry.metadata).await?;
        let obj = entry.path.name().unwrap_or("").to_string();
        children.push((
            format!("/remote.php/dav/calendars/{user}/{calendar_id}/{obj}"),
            etag,
        ));
    }
    Ok(children)
}
