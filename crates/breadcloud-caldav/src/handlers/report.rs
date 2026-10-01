use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{CalendarStore, DavPath, StorageError};
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::{read_body, xml_response};
use crate::ical;
use crate::path::CalPath;
use crate::state::AppState;
use crate::xml::request::{parse_report_body, PropSet, ReportRequest, TimeRange};
use crate::xml::response::{self, EventEntry, SyncEntry};

fn multiget_path(href: &str) -> Option<String> {
    let uri = href.parse::<Uri>().ok()?;
    let path = percent_encoding::percent_decode_str(uri.path())
        .decode_utf8()
        .ok()?;
    Some(path.into_owned())
}

fn event_href(user: &str, calendar_id: &str, obj: &str) -> String {
    format!("/remote.php/dav/calendars/{user}/{calendar_id}/{obj}")
}

/// One sync-collection diff entry built up before rendering: href,
/// whether it is a deletion, its etag (`None` for a deletion), and its
/// `.ics` bytes if `calendar-data` was requested (`None` otherwise, or
/// for a deletion).
type SyncBuiltEntry = (String, bool, Option<String>, Option<Vec<u8>>);

/// `REPORT` on a calendar collection: dispatches to whichever of the
/// three shapes real clients send (RFC 4791 `calendar-query`/
/// `calendar-multiget`, RFC 6578 `sync-collection`) -- see
/// `crate::xml::request::parse_report_body`.
pub async fn handle(state: &AppState, user: &str, calendar_id: &str, req: Request) -> Response {
    if !state.calendars.calendar_exists(user, calendar_id).await {
        return (StatusCode::NOT_FOUND, "no such calendar").into_response();
    }

    let body = read_body(req).await;
    match parse_report_body(&body) {
        ReportRequest::CalendarMultiget { hrefs, props } => {
            calendar_multiget(state, user, calendar_id, &hrefs, &props).await
        }
        ReportRequest::CalendarQuery {
            component,
            time_range,
            props,
        } => {
            calendar_query(
                state,
                user,
                calendar_id,
                component.as_deref(),
                time_range,
                &props,
            )
            .await
        }
        ReportRequest::SyncCollection { sync_token, props } => {
            sync_collection(state, user, calendar_id, sync_token.as_deref(), &props).await
        }
        ReportRequest::Unsupported => {
            (StatusCode::BAD_REQUEST, "unsupported REPORT type").into_response()
        }
    }
}

/// RFC 4791 7.9: fetch an explicit, known set of calendar objects by
/// href. Absolute hrefs and percent-encoded path segments are normalized
/// before parsing. Missing or out-of-scope hrefs receive a 404 response.
async fn calendar_multiget(
    state: &AppState,
    user: &str,
    calendar_id: &str,
    hrefs: &[String],
    props: &PropSet,
) -> Response {
    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };

    let mut fetched: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut missing = Vec::new();
    for href in hrefs {
        let Some(path) = multiget_path(href) else {
            continue;
        };
        let Some(CalPath::Event(href_user, href_cal, obj)) = CalPath::parse(&path) else {
            missing.push(path);
            continue;
        };
        if href_user != user || href_cal != calendar_id {
            missing.push(path);
            continue;
        }
        let Ok(dav) = DavPath::parse(&format!("/{obj}")) else {
            missing.push(path);
            continue;
        };
        let bytes = match events.read_bytes(&dav).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound) => {
                missing.push(path);
                continue;
            }
            Err(err) => return storage_error_response(err),
        };
        let etag = match events.etag(&dav).await {
            Ok(etag) => etag,
            Err(StorageError::NotFound) => {
                missing.push(path);
                continue;
            }
            Err(err) => return storage_error_response(err),
        };
        fetched.push((event_href(user, calendar_id, &obj), etag, bytes));
    }

    let entries: Vec<EventEntry> = fetched
        .iter()
        .map(|(href, etag, ics)| EventEntry {
            href: href.clone(),
            etag: etag.as_str(),
            ics: if props.calendar_data {
                Some(ics.as_slice())
            } else {
                None
            },
        })
        .collect();
    xml_response(response::calendar_object_multistatus_with_missing(
        &entries, &missing, props,
    ))
}

/// RFC 4791 7.8: fetch every calendar object matching an optional
/// component-type and time-range filter. No index -- a linear scan of
/// the calendar's events, which is fine at this deployment's real scale
/// (a few hundred events per calendar; see the workspace README).
async fn calendar_query(
    state: &AppState,
    user: &str,
    calendar_id: &str,
    component: Option<&str>,
    time_range: Option<TimeRange>,
    props: &PropSet,
) -> Response {
    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    let listing = match events.list_dir(&DavPath::root()).await {
        Ok(l) => l,
        Err(err) => return storage_error_response(err),
    };

    let mut matched: Vec<(String, String, Vec<u8>)> = Vec::new();
    for entry in listing {
        if entry.metadata.is_dir() {
            continue;
        }
        let Ok(bytes) = events.read_bytes(&entry.path).await else {
            continue;
        };
        if let Some(comp) = component {
            if !ical::has_component(&bytes, comp) {
                continue;
            }
        }
        if let Some(tr) = &time_range {
            if !ical::time_range_overlaps(&bytes, tr.start.as_deref(), tr.end.as_deref()) {
                continue;
            }
        }
        let Ok(etag) = events.etag_for(&entry.path, &entry.metadata).await else {
            continue;
        };
        let obj = entry.path.name().unwrap_or("").to_string();
        matched.push((event_href(user, calendar_id, &obj), etag, bytes));
    }

    let entries: Vec<EventEntry> = matched
        .iter()
        .map(|(href, etag, ics)| EventEntry {
            href: href.clone(),
            etag: etag.as_str(),
            ics: if props.calendar_data {
                Some(ics.as_slice())
            } else {
                None
            },
        })
        .collect();
    xml_response(response::calendar_object_multistatus(&entries, props))
}

/// RFC 6578: incremental sync. `sync_token = None` is an initial sync
/// (every live event, no tombstones); `Some(token)` replays the changelog
/// since that token (changed resources as 200, removed ones as 404) --
/// see `breadcloud_storage::CalendarStore::changes_since`'s doc comment
/// for why an unbounded initial sync and an incremental one need
/// different tombstone handling.
async fn sync_collection(
    state: &AppState,
    user: &str,
    calendar_id: &str,
    sync_token: Option<&str>,
    props: &PropSet,
) -> Response {
    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };

    let since = sync_token.and_then(CalendarStore::parse_sync_token);
    let (changes, new_ctag) = match state
        .calendars
        .changes_since(user, calendar_id, since)
        .await
    {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

    let initial = sync_token.is_none();

    let mut built: Vec<SyncBuiltEntry> = Vec::new();
    for change in changes {
        if initial && change.deleted {
            continue;
        }
        let href = event_href(user, calendar_id, &change.uid);
        if change.deleted {
            built.push((href, true, None, None));
            continue;
        }
        let Ok(dav) = DavPath::parse(&format!("/{}", change.uid)) else {
            continue;
        };
        match events.etag(&dav).await {
            Ok(etag) => {
                let ics = if props.calendar_data {
                    events.read_bytes(&dav).await.ok()
                } else {
                    None
                };
                built.push((href, false, Some(etag), ics));
            }
            // Recorded as changed but no longer present on disk (e.g.
            // deleted again after this changelog window) -- report it as
            // removed instead of a broken 200 with no etag.
            Err(_) => built.push((href, true, None, None)),
        }
    }

    let entries: Vec<SyncEntry> = built
        .iter()
        .map(|(href, deleted, etag, ics)| SyncEntry {
            href: href.clone(),
            deleted: *deleted,
            etag: etag.as_deref(),
            ics: ics.as_deref(),
        })
        .collect();

    let new_token = CalendarStore::format_sync_token(&new_ctag);
    xml_response(response::sync_collection_multistatus(
        &entries, props, &new_token,
    ))
}
