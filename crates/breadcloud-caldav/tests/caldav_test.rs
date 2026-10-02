//! End-to-end tests against the real router (`tower::ServiceExt::oneshot`,
//! no socket needed), simulating the request sequences a real CalDAV
//! client actually drives: PROPFIND discovery (current-user-principal ->
//! principal -> calendar-home-set listing), REPORT calendar-query /
//! calendar-multiget, PUT/GET/DELETE on individual events, and REPORT
//! sync-collection for incremental sync. See the workspace README for
//! which parts of this were also verified against a real client/tool and
//! which were not.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_caldav::{router, AppState};
use breadcloud_storage::CalendarStore;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

async fn test_app() -> (TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let calendars = CalendarStore::new(dir.path());
    calendars
        .create_calendar("dev", "personal", "Personal", Some("#ff0000"))
        .await
        .unwrap();
    calendars
        .create_calendar("dev", "family", "Family", None)
        .await
        .unwrap();
    let state = AppState::new(calendars);
    (dir, router(state))
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn sample_event(uid: &str, dtstart: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:{dtstart}\r\nDTEND:{dtstart}\r\nSUMMARY:Test event {uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn propfind(app: &axum::Router, uri: &str, depth: &str) -> axum::response::Response {
    let req = Request::builder()
        .method("PROPFIND")
        .uri(uri)
        .header("Depth", depth)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn put_event(app: &axum::Router, uri: &str, ics: &str) -> axum::response::Response {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .body(Body::from(ics.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn current_user_principal_discovery() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("<d:current-user-principal>"));
    assert!(body.contains("/remote.php/dav/principals/anonymous/</d:href>"));
}

#[tokio::test]
async fn principal_reports_calendar_home_set() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/principals/dev/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("<cal:calendar-home-set>"));
    assert!(body.contains("/remote.php/dav/calendars/dev/</d:href>"));
}

#[tokio::test]
async fn calendar_home_depth_1_lists_all_calendars() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/calendars/dev/", "1").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("personal/</d:href>"));
    assert!(body.contains("family/</d:href>"));
    assert!(body.contains("<cal:calendar/>"));
}

#[tokio::test]
async fn calendar_home_depth_0_omits_children() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/calendars/dev/", "0").await;
    let body = body_string(response).await;
    assert!(!body.contains("personal/</d:href>"));
}

/// A real CalDAV client (vdirsyncer's CalDAV storage backend, confirmed
/// live -- see the workspace README) uses PROPFIND Depth 1 on a calendar
/// collection itself, not REPORT, to enumerate its events. Without event
/// children in that response, vdirsyncer saw the calendar as always
/// empty even with events present -- this pins that regression.
#[tokio::test]
async fn calendar_propfind_depth_1_lists_event_children() {
    let (_dir, app) = test_app().await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/one.ics",
        &sample_event("one", "20260115T090000Z"),
    )
    .await;

    let depth0 = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "0").await;
    let depth0_body = body_string(depth0).await;
    assert!(!depth0_body.contains("one.ics"));

    let depth1 = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "1").await;
    let depth1_body = body_string(depth1).await;
    assert!(depth1_body.contains("one.ics</d:href>"));
    assert!(depth1_body.contains("<d:getetag>"));
}

#[tokio::test]
async fn mkcalendar_creates_a_new_calendar() {
    let (_dir, app) = test_app().await;
    let body = r#"<?xml version="1.0"?>
<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:set><d:prop><d:displayname>School</d:displayname></d:prop></d:set>
</c:mkcalendar>"#;
    let req = Request::builder()
        .method("MKCALENDAR")
        .uri("/remote.php/dav/calendars/dev/school/")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = propfind(&app, "/remote.php/dav/calendars/dev/school/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let listing = propfind(&app, "/remote.php/dav/calendars/dev/", "1").await;
    let listing_body = body_string(listing).await;
    assert!(listing_body.contains("<d:displayname>School</d:displayname>"));
}

#[tokio::test]
async fn mkcalendar_on_existing_id_is_405() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("MKCALENDAR")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn put_then_get_roundtrips_event_content() {
    let (_dir, app) = test_app().await;
    let ics = sample_event("evt-1", "20260115T090000Z");

    let response = put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/evt-1.ics",
        &ics,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(response.headers().contains_key("etag"));

    let get = Request::builder()
        .method("GET")
        .uri("/remote.php/dav/calendars/dev/personal/evt-1.ics")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/calendar; charset=utf-8"
    );
    let body = body_string(response).await;
    assert_eq!(body, ics);
}

#[tokio::test]
async fn put_overwrite_returns_no_content() {
    let (_dir, app) = test_app().await;
    let ics = sample_event("evt-1", "20260115T090000Z");
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/evt-1.ics",
        &ics,
    )
    .await;
    let response = put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/evt-1.ics",
        &ics,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_without_uid_is_bad_request() {
    let (_dir, app) = test_app().await;
    let response = put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/broken.ics",
        "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_rejects_control_characters_in_event_names_before_writing() {
    let (dir, app) = test_app().await;
    for encoded_name in ["bad%0Aname.ics", "bad%09name.ics"] {
        let response = put_event(
            &app,
            &format!("/remote.php/dav/calendars/dev/personal/{encoded_name}"),
            &sample_event("evt-1", "20260115T090000Z"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let calendars = CalendarStore::new(dir.path());
    assert_eq!(calendars.ctag("dev", "personal").await.unwrap(), "0");
    let events = calendars.events_storage("dev", "personal").await.unwrap();
    for name in ["bad\nname.ics", "bad\tname.ics"] {
        let path = breadcloud_storage::DavPath::parse(&format!("/{name}")).unwrap();
        assert!(!events.exists(&path).await);
    }
}

#[tokio::test]
async fn put_into_a_nonexistent_calendar_is_conflict() {
    let (_dir, app) = test_app().await;
    let ics = sample_event("evt-1", "20260115T090000Z");
    let response = put_event(&app, "/remote.php/dav/calendars/dev/nosuch/evt-1.ics", &ics).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_removes_the_event() {
    let (_dir, app) = test_app().await;
    let ics = sample_event("evt-1", "20260115T090000Z");
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/evt-1.ics",
        &ics,
    )
    .await;

    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/calendars/dev/personal/evt-1.ics")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(del).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let get = Request::builder()
        .method("GET")
        .uri("/remote.php/dav/calendars/dev/personal/evt-1.ics")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ctag_changes_on_every_event_mutation() {
    let (_dir, app) = test_app().await;

    let initial = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "0").await;
    let initial_body = body_string(initial).await;
    assert!(initial_body.contains("<cs:getctag>0</cs:getctag>"));

    let ics = sample_event("evt-1", "20260115T090000Z");
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/evt-1.ics",
        &ics,
    )
    .await;

    let after_put = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "0").await;
    let after_put_body = body_string(after_put).await;
    assert!(after_put_body.contains("<cs:getctag>1</cs:getctag>"));

    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/calendars/dev/personal/evt-1.ics")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(del).await.unwrap();

    let after_delete = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "0").await;
    let after_delete_body = body_string(after_delete).await;
    assert!(after_delete_body.contains("<cs:getctag>2</cs:getctag>"));
}

#[tokio::test]
async fn calendar_query_time_range_filters_events() {
    let (_dir, app) = test_app().await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/january.ics",
        &sample_event("january", "20260115T090000Z"),
    )
    .await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/march.ics",
        &sample_event("march", "20260315T090000Z"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="20260101T000000Z" end="20260201T000000Z"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("january.ics"));
    assert!(!body.contains("march.ics"));
    assert!(body.contains("SUMMARY:Test event january"));
}

#[tokio::test]
async fn calendar_multiget_fetches_explicit_hrefs() {
    let (_dir, app) = test_app().await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/one.ics",
        &sample_event("one", "20260115T090000Z"),
    )
    .await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/two.ics",
        &sample_event("two", "20260116T090000Z"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <d:href>/remote.php/dav/calendars/dev/personal/one.ics</d:href>
</c:calendar-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    assert!(body.contains("one.ics"));
    assert!(!body.contains("two.ics"));
}

#[tokio::test]
async fn calendar_multiget_decodes_absolute_hrefs_and_reports_missing_objects() {
    let (_dir, app) = test_app().await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/space%20name.ics",
        &sample_event("space", "20260115T090000Z"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <d:href>https://dav.example.test/remote.php/dav/calendars/dev/personal/space%20name.ics</d:href>
  <d:href>https://dav.example.test/remote.php/dav/calendars/dev/personal/missing%20event.ics</d:href>
</c:calendar-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("space%20name.ics"));
    assert!(body.contains("SUMMARY:Test event space"));
    assert!(body.contains("missing%20event.ics"));
    assert!(body.contains("HTTP/1.1 404 Not Found"));
}

#[tokio::test]
async fn calendar_multiget_reports_non_missing_storage_errors() {
    let (dir, app) = test_app().await;
    std::fs::create_dir_all(dir.path().join("dev/personal/events/directory.ics")).unwrap();
    let report_body = r#"<?xml version="1.0"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/></d:prop>
  <d:href>/remote.php/dav/calendars/dev/personal/directory.ics</d:href>
</c:calendar-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn sync_collection_initial_then_incremental() {
    let (_dir, app) = test_app().await;
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/one.ics",
        &sample_event("one", "20260115T090000Z"),
    )
    .await;

    // Initial sync: no token, gets everything live, plus a fresh token.
    let initial_body = r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(initial_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("one.ics"));
    assert!(!body.contains("404 Not Found"));
    let token_start = body.find("<d:sync-token>").unwrap() + "<d:sync-token>".len();
    let token_end = body[token_start..].find("</d:sync-token>").unwrap() + token_start;
    let token = &body[token_start..token_end];
    assert_eq!(token, "breadcloud-sync:1");

    // Add a second event and delete the first.
    put_event(
        &app,
        "/remote.php/dav/calendars/dev/personal/two.ics",
        &sample_event("two", "20260116T090000Z"),
    )
    .await;
    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/calendars/dev/personal/one.ics")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(del).await.unwrap();

    // Incremental sync from the token above should show two.ics created
    // and one.ics gone (404), not a full listing.
    let incremental_body = format!(
        r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>{token}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#
    );
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::from(incremental_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    assert!(body.contains("two.ics"));
    assert!(body.contains("one.ics</d:href>\n    <d:status>HTTP/1.1 404 Not Found</d:status>"));
    assert!(body.contains("<d:sync-token>breadcloud-sync:3</d:sync-token>"));
}

#[tokio::test]
async fn sync_collection_rejects_malformed_and_future_tokens() {
    let (_dir, app) = test_app().await;
    for token in ["not-a-sync-token", "breadcloud-sync:1"] {
        let report = format!(
            "<d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{token}</d:sync-token><d:prop><d:getetag/></d:prop></d:sync-collection>"
        );
        let req = Request::builder()
            .method("REPORT")
            .uri("/remote.php/dav/calendars/dev/personal/")
            .body(Body::from(report))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "token: {token}");
        let body = body_string(response).await;
        assert!(
            body.contains("<d:valid-sync-token/>"),
            "token: {token}, body: {body}"
        );
    }
}

#[tokio::test]
async fn basic_auth_rejects_missing_and_wrong_credentials() {
    let dir = TempDir::new().unwrap();
    let calendars = CalendarStore::new(dir.path());
    calendars
        .create_calendar("dev", "personal", "Personal", None)
        .await
        .unwrap();
    let state = AppState::new(calendars).with_auth("dev", "secret");
    let app = router(state);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/calendars/dev/")
        .header("Depth", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/calendars/dev/")
        .header("Depth", "1")
        .header("Authorization", "Basic d3Jvbmc6d3Jvbmc=") // wrong:wrong
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/calendars/dev/")
        .header("Depth", "1")
        .header("Authorization", "Basic ZGV2OnNlY3JldA==") // dev:secret
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
}

#[tokio::test]
async fn well_known_caldav_redirects_to_dav_root() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/caldav")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(
        response.headers().get("location").unwrap(),
        "/remote.php/dav/"
    );
}

#[tokio::test]
async fn options_advertises_calendar_access() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/remote.php/dav/calendars/dev/personal/")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("dav")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("calendar-access"));
}

/// A full realistic client sequence in one test, mirroring what DAVx5 or
/// iOS's Calendar app actually drives end to end: discover the principal
/// and calendar home, list calendars, create one via MKCALENDAR, PUT a
/// few events into it, fetch them back via calendar-query, then do an
/// initial and an incremental sync-collection.
#[tokio::test]
async fn full_discovery_to_sync_sequence() {
    let (_dir, app) = test_app().await;

    let principal = propfind(&app, "/remote.php/dav/", "0").await;
    let principal_body = body_string(principal).await;
    assert!(principal_body.contains("/remote.php/dav/principals/anonymous/"));

    let home_set = propfind(&app, "/remote.php/dav/principals/anonymous/", "0").await;
    let home_set_body = body_string(home_set).await;
    assert!(home_set_body.contains("/remote.php/dav/calendars/anonymous/"));

    let mkcal = Request::builder()
        .method("MKCALENDAR")
        .uri("/remote.php/dav/calendars/anonymous/school/")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcal).await.unwrap().status(),
        StatusCode::CREATED
    );

    for (uid, dt) in [
        ("a", "20260201T090000Z"),
        ("b", "20260202T090000Z"),
        ("c", "20260203T090000Z"),
    ] {
        let response = put_event(
            &app,
            &format!("/remote.php/dav/calendars/anonymous/school/{uid}.ics"),
            &sample_event(uid, dt),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let listing = propfind(&app, "/remote.php/dav/calendars/anonymous/", "1").await;
    let listing_body = body_string(listing).await;
    assert!(listing_body.contains("school/</d:href>"));

    let query_body = r#"<?xml version="1.0"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"/></c:comp-filter></c:filter>
</c:calendar-query>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/anonymous/school/")
        .body(Body::from(query_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    for uid in ["a", "b", "c"] {
        assert!(body.contains(&format!("{uid}.ics")));
    }

    let sync_body = r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/calendars/anonymous/school/")
        .body(Body::from(sync_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    for uid in ["a", "b", "c"] {
        assert!(body.contains(&format!("{uid}.ics")));
    }
    assert!(body.contains("<d:sync-token>breadcloud-sync:3</d:sync-token>"));
}
