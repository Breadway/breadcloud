use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, WriteOutcome};
use breadcloud_webdav::error::storage_error_response;

use crate::handlers::read_body;
use crate::ical;
use crate::state::AppState;

fn dav_path(obj: &str) -> Option<DavPath> {
    DavPath::parse(&format!("/{obj}")).ok()
}

/// `GET /remote.php/dav/calendars/<user>/<calendar id>/<object>`: fetch
/// one event's raw iCalendar content. Not streamed (unlike
/// `breadcloud_webdav::handlers::get`'s file GET): a single `.ics` object
/// is at most a few kilobytes, nowhere near the file tree's
/// multi-gigabyte case that streaming exists for there.
pub async fn get(state: &AppState, user: &str, calendar_id: &str, obj: &str) -> Response {
    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };
    let bytes = match events.read_bytes(&path).await {
        Ok(b) => b,
        Err(err) => return storage_error_response(err),
    };
    let etag = match events.etag(&path).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/calendar; charset=utf-8")
        .header("ETag", etag)
        .body(Body::from(bytes))
        .unwrap()
}

/// `PUT /remote.php/dav/calendars/<user>/<calendar id>/<object>`: create
/// or overwrite one event. Requires the body to at least parse a `UID`
/// (RFC 5545 requires one on every `VEVENT`/`VTODO`); does not require
/// the URL's `<object>` segment to equal that UID plus `.ics` -- real
/// clients always send them matching, but nothing in RFC 4791 requires a
/// server to reject a mismatch, and being lenient here costs nothing
/// (the URL segment, not the UID, is what this server actually addresses
/// the object by -- see `breadcloud_storage::CalendarStore`'s module doc
/// comment).
pub async fn put(
    state: &AppState,
    user: &str,
    calendar_id: &str,
    obj: &str,
    req: Request,
) -> Response {
    if !state.calendars.calendar_exists(user, calendar_id).await {
        // RFC 4918 9.7.1 (inherited by CalDAV): PUT into a collection
        // that does not exist is 409 Conflict, not 404 -- same status
        // breadcloud_webdav::handlers::put gives a missing parent.
        return (StatusCode::CONFLICT, "no such calendar").into_response();
    }
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };

    let bytes = read_body(req).await;
    if ical::extract_uid(&bytes).is_none() {
        return (StatusCode::BAD_REQUEST, "iCalendar object has no UID").into_response();
    }

    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    let (outcome, etag) = match events.write_bytes(&path, &bytes).await {
        Ok(v) => v,
        Err(err) => return storage_error_response(err),
    };

    if let Err(err) = state
        .calendars
        .record_change(user, calendar_id, obj, false)
        .await
    {
        tracing::warn!(
            "failed to record calendar change for {user}/{calendar_id}/{obj}: {err}; \
             the event itself was written successfully, but getctag/sync-collection may \
             lag until the next change is recorded"
        );
    }

    let status = match outcome {
        WriteOutcome::Created => StatusCode::CREATED,
        WriteOutcome::Overwritten => StatusCode::NO_CONTENT,
    };
    Response::builder()
        .status(status)
        .header("ETag", etag)
        .body(Body::empty())
        .unwrap()
}

/// `DELETE /remote.php/dav/calendars/<user>/<calendar id>/<object>`.
/// Unlike `breadcloud_webdav`'s DELETE, this does not go through trash
/// (CLOUD-5): trash is a file-tree concept keyed to `Storage`/`DavPath`,
/// and calendar objects are a deliberately separate, simpler resource
/// model with no versioning or soft-delete asked for by this issue --
/// see the workspace README.
pub async fn delete(state: &AppState, user: &str, calendar_id: &str, obj: &str) -> Response {
    let events = match state.calendars.events_storage(user, calendar_id).await {
        Ok(e) => e,
        Err(err) => return storage_error_response(err),
    };
    let Some(path) = dav_path(obj) else {
        return (StatusCode::BAD_REQUEST, "invalid resource name").into_response();
    };
    match events.delete(&path).await {
        Ok(()) => {
            if let Err(err) = state
                .calendars
                .record_change(user, calendar_id, obj, true)
                .await
            {
                tracing::warn!(
                    "failed to record calendar deletion for {user}/{calendar_id}/{obj}: {err}"
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => storage_error_response(err),
    }
}
