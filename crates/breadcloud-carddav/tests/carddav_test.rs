//! End-to-end tests against the real router (`tower::ServiceExt::oneshot`,
//! no socket needed), simulating the request sequences a real CardDAV
//! client actually drives: addressbook-home-set / addressbook listing,
//! REPORT addressbook-query / addressbook-multiget, PUT/GET/DELETE on
//! individual contacts (including a group vCard), and REPORT
//! sync-collection for incremental sync. This crate's standalone router
//! does not include principal/current-user-principal discovery (that
//! stays `breadcloud-caldav`'s -- see this crate's `lib.rs` doc comment);
//! `breadcloud-server`'s combined-app tests cover that hop. See the
//! docs/DESIGN.md for which parts of this were also verified against a
//! real client/tool and which were not.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_carddav::{router, AppState};
use breadcloud_storage::AddressbookStore;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

async fn test_app() -> (TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let addressbooks = AddressbookStore::new(dir.path());
    addressbooks
        .create_addressbook("dev", "contacts", "Contacts")
        .await
        .unwrap();
    let state = AppState::new(addressbooks);
    (dir, router(state))
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn sample_contact(uid: &str, fn_value: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{fn_value}\r\nEND:VCARD\r\n")
}

fn group_contact(uid: &str, name: &str, member_uids: &[&str]) -> String {
    let mut s = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{name}\r\nKIND:group\r\n");
    for m in member_uids {
        s.push_str(&format!("MEMBER:urn:uuid:{m}\r\n"));
    }
    s.push_str("END:VCARD\r\n");
    s
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

async fn put_contact(app: &axum::Router, uri: &str, vcf: &str) -> axum::response::Response {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .body(Body::from(vcf.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn addressbook_home_depth_1_lists_addressbooks() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/addressbooks/dev/", "1").await;
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("contacts/</d:href>"));
    assert!(body.contains("<card:addressbook/>"));
}

#[tokio::test]
async fn addressbook_home_depth_0_omits_children() {
    let (_dir, app) = test_app().await;
    let response = propfind(&app, "/remote.php/dav/addressbooks/dev/", "0").await;
    let body = body_string(response).await;
    assert!(!body.contains("contacts/</d:href>"));
}

/// Same real-client-discovered regression `breadcloud-caldav` pins for
/// calendars, replicated here: vdirsyncer's storage backend uses PROPFIND
/// Depth 1 on the collection itself to enumerate objects, not just
/// REPORT.
#[tokio::test]
async fn addressbook_propfind_depth_1_lists_contact_children() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/one.vcf",
        &sample_contact("one", "Ada Lovelace"),
    )
    .await;

    let depth0 = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "0").await;
    let depth0_body = body_string(depth0).await;
    assert!(!depth0_body.contains("one.vcf"));

    let depth1 = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "1").await;
    let depth1_body = body_string(depth1).await;
    assert!(depth1_body.contains("one.vcf</d:href>"));
    assert!(depth1_body.contains("<d:getetag>"));
}

#[tokio::test]
async fn mkcol_creates_a_new_addressbook() {
    let (_dir, app) = test_app().await;
    let body = r#"<?xml version="1.0"?>
<d:mkcol xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
  <d:set><d:prop>
    <d:resourcetype><d:collection/><card:addressbook/></d:resourcetype>
    <d:displayname>Work</d:displayname>
  </d:prop></d:set>
</d:mkcol>"#;
    let req = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/addressbooks/dev/work/")
        .body(Body::from(body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = propfind(&app, "/remote.php/dav/addressbooks/dev/work/", "0").await;
    assert_eq!(response.status().as_u16(), 207);
    let listing = propfind(&app, "/remote.php/dav/addressbooks/dev/", "1").await;
    let listing_body = body_string(listing).await;
    assert!(listing_body.contains("<d:displayname>Work</d:displayname>"));
}

#[tokio::test]
async fn mkcol_on_existing_id_is_405() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn plain_mkcol_with_no_body_still_creates_an_addressbook() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/addressbooks/dev/plain/")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn put_then_get_roundtrips_contact_content() {
    let (_dir, app) = test_app().await;
    let vcf = sample_contact("ctc-1", "Ada Lovelace");

    let response = put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf",
        &vcf,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(response.headers().contains_key("etag"));

    let get = Request::builder()
        .method("GET")
        .uri("/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/vcard; charset=utf-8"
    );
    let body = body_string(response).await;
    assert_eq!(body, vcf);
}

#[tokio::test]
async fn put_overwrite_returns_no_content() {
    let (_dir, app) = test_app().await;
    let vcf = sample_contact("ctc-1", "Ada Lovelace");
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf",
        &vcf,
    )
    .await;
    let response = put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf",
        &vcf,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn put_without_uid_is_bad_request() {
    let (_dir, app) = test_app().await;
    let response = put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/broken.vcf",
        "BEGIN:VCARD\r\nFN:No Uid\r\nEND:VCARD\r\n",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_rejects_control_characters_in_contact_names_before_writing() {
    let (dir, app) = test_app().await;
    for encoded_name in ["bad%0Aname.vcf", "bad%09name.vcf"] {
        let response = put_contact(
            &app,
            &format!("/remote.php/dav/addressbooks/dev/contacts/{encoded_name}"),
            &sample_contact("ctc-1", "Ada Lovelace"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let addressbooks = AddressbookStore::new(dir.path());
    assert_eq!(addressbooks.ctag("dev", "contacts").await.unwrap(), "0");
    let contacts = addressbooks
        .contacts_storage("dev", "contacts")
        .await
        .unwrap();
    for name in ["bad\nname.vcf", "bad\tname.vcf"] {
        let path = breadcloud_storage::DavPath::parse(&format!("/{name}")).unwrap();
        assert!(!contacts.exists(&path).await);
    }
}

#[tokio::test]
async fn put_into_a_nonexistent_addressbook_is_conflict() {
    let (_dir, app) = test_app().await;
    let vcf = sample_contact("ctc-1", "Ada Lovelace");
    let response = put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/nosuch/ctc-1.vcf",
        &vcf,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn delete_removes_the_contact() {
    let (_dir, app) = test_app().await;
    let vcf = sample_contact("ctc-1", "Ada Lovelace");
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf",
        &vcf,
    )
    .await;

    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(del).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let get = Request::builder()
        .method("GET")
        .uri("/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ctag_changes_on_every_contact_mutation() {
    let (_dir, app) = test_app().await;

    let initial = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "0").await;
    let initial_body = body_string(initial).await;
    assert!(initial_body.contains("<cs:getctag>0</cs:getctag>"));

    let vcf = sample_contact("ctc-1", "Ada Lovelace");
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf",
        &vcf,
    )
    .await;

    let after_put = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "0").await;
    let after_put_body = body_string(after_put).await;
    assert!(after_put_body.contains("<cs:getctag>1</cs:getctag>"));

    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/addressbooks/dev/contacts/ctc-1.vcf")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(del).await.unwrap();

    let after_delete = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "0").await;
    let after_delete_body = body_string(after_delete).await;
    assert!(after_delete_body.contains("<cs:getctag>2</cs:getctag>"));
}

#[tokio::test]
async fn addressbook_query_text_match_filters_contacts() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ada.vcf",
        &sample_contact("ada", "Ada Lovelace"),
    )
    .await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/grace.vcf",
        &sample_contact("grace", "Grace Hopper"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <c:filter>
    <c:prop-filter name="FN"><c:text-match>Lovelace</c:text-match></c:prop-filter>
  </c:filter>
</c:addressbook-query>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("ada.vcf"));
    assert!(!body.contains("grace.vcf"));
    assert!(body.contains("FN:Ada Lovelace"));
}

#[tokio::test]
async fn addressbook_query_without_filter_returns_all_contacts() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ada.vcf",
        &sample_contact("ada", "Ada Lovelace"),
    )
    .await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/grace.vcf",
        &sample_contact("grace", "Grace Hopper"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/></d:prop>
  <c:filter/>
</c:addressbook-query>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    assert!(body.contains("ada.vcf"));
    assert!(body.contains("grace.vcf"));
}

#[tokio::test]
async fn addressbook_multiget_fetches_explicit_hrefs() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/one.vcf",
        &sample_contact("one", "Ada Lovelace"),
    )
    .await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/two.vcf",
        &sample_contact("two", "Grace Hopper"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <d:href>/remote.php/dav/addressbooks/dev/contacts/one.vcf</d:href>
</c:addressbook-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    assert!(body.contains("one.vcf"));
    assert!(!body.contains("two.vcf"));
}

#[tokio::test]
async fn addressbook_multiget_decodes_absolute_hrefs_and_reports_missing_objects() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/space%20name.vcf",
        &sample_contact("space", "Ada Lovelace"),
    )
    .await;

    let report_body = r#"<?xml version="1.0"?>
<c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
  <d:href>https://dav.example.test/remote.php/dav/addressbooks/dev/contacts/space%20name.vcf</d:href>
  <d:href>https://dav.example.test/remote.php/dav/addressbooks/dev/contacts/missing%20contact.vcf</d:href>
</c:addressbook-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("space%20name.vcf"));
    assert!(body.contains("FN:Ada Lovelace"));
    assert!(body.contains("missing%20contact.vcf"));
    assert!(body.contains("HTTP/1.1 404 Not Found"));
}

#[tokio::test]
async fn addressbook_multiget_reports_non_missing_storage_errors() {
    let (dir, app) = test_app().await;
    std::fs::create_dir_all(dir.path().join("dev/contacts/contacts/directory.vcf")).unwrap();
    let report_body = r#"<?xml version="1.0"?>
<c:addressbook-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/></d:prop>
  <d:href>/remote.php/dav/addressbooks/dev/contacts/directory.vcf</d:href>
</c:addressbook-multiget>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(report_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

/// The real data this issue targets is 76 contacts split across groups
/// (Friends/ICE/starred/Family team); groups are ordinary vCards with
/// `KIND:group` and `MEMBER:urn:uuid:<uid>` lines, not a separate
/// protocol concept (see `breadcloud_carddav::vcard`'s doc comment). This
/// pins that a group vCard round-trips through PUT/GET/REPORT exactly
/// like any other contact.
#[tokio::test]
async fn group_vcard_round_trips_like_any_other_contact() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/ada.vcf",
        &sample_contact("ada", "Ada Lovelace"),
    )
    .await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/grace.vcf",
        &sample_contact("grace", "Grace Hopper"),
    )
    .await;
    let group = group_contact("friends-group", "Friends", &["ada", "grace"]);
    let response = put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/friends-group.vcf",
        &group,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let get = Request::builder()
        .method("GET")
        .uri("/remote.php/dav/addressbooks/dev/contacts/friends-group.vcf")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(body.contains("KIND:group"));
    assert!(body.contains("MEMBER:urn:uuid:ada"));
    assert!(body.contains("MEMBER:urn:uuid:grace"));

    // Depth 1 enumeration of the addressbook lists the group alongside
    // ordinary contacts -- no separate surface for it.
    let listing = propfind(&app, "/remote.php/dav/addressbooks/dev/contacts/", "1").await;
    let listing_body = body_string(listing).await;
    assert!(listing_body.contains("friends-group.vcf"));
    assert!(listing_body.contains("ada.vcf"));
}

#[tokio::test]
async fn sync_collection_initial_then_incremental() {
    let (_dir, app) = test_app().await;
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/one.vcf",
        &sample_contact("one", "Ada Lovelace"),
    )
    .await;

    let initial_body = r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token/>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(initial_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let body = body_string(response).await;
    assert!(body.contains("one.vcf"));
    assert!(!body.contains("404 Not Found"));
    let token_start = body.find("<d:sync-token>").unwrap() + "<d:sync-token>".len();
    let token_end = body[token_start..].find("</d:sync-token>").unwrap() + token_start;
    let token = &body[token_start..token_end];
    assert_eq!(token, "breadcloud-sync:1");

    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/two.vcf",
        &sample_contact("two", "Grace Hopper"),
    )
    .await;
    let del = Request::builder()
        .method("DELETE")
        .uri("/remote.php/dav/addressbooks/dev/contacts/one.vcf")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(del).await.unwrap();

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
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(incremental_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    assert!(body.contains("two.vcf"));
    assert!(body.contains("one.vcf</d:href>\n    <d:status>HTTP/1.1 404 Not Found</d:status>"));
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
            .uri("/remote.php/dav/addressbooks/dev/contacts/")
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
    let addressbooks = AddressbookStore::new(dir.path());
    addressbooks
        .create_addressbook("dev", "contacts", "Contacts")
        .await
        .unwrap();
    let state = AppState::new(addressbooks).with_auth("dev", "secret");
    let app = router(state);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/addressbooks/dev/")
        .header("Depth", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/addressbooks/dev/")
        .header("Depth", "1")
        .header("Authorization", "Basic d3Jvbmc6d3Jvbmc=") // wrong:wrong
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/addressbooks/dev/")
        .header("Depth", "1")
        .header("Authorization", "Basic ZGV2OnNlY3JldA==") // dev:secret
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
}

#[tokio::test]
async fn well_known_carddav_redirects_to_dav_root() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/.well-known/carddav")
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
async fn options_advertises_addressbook_class() {
    let (_dir, app) = test_app().await;
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
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
        .contains("addressbook"));
}

/// A full realistic client sequence in one test, mirroring what DAVx5 or
/// iOS Contacts actually drives end to end (minus the
/// current-user-principal hop, which stays `breadcloud-caldav`'s --
/// see this file's module doc comment): list addressbooks, PUT a handful
/// of contacts including a group, fetch them back via addressbook-query,
/// then do an initial and an incremental sync-collection.
#[tokio::test]
async fn full_listing_to_sync_sequence() {
    let (_dir, app) = test_app().await;

    let listing = propfind(&app, "/remote.php/dav/addressbooks/dev/", "1").await;
    let listing_body = body_string(listing).await;
    assert!(listing_body.contains("contacts/</d:href>"));

    for (uid, fname) in [
        ("a", "Alice Anderson"),
        ("b", "Bob Baker"),
        ("c", "Carol Clark"),
    ] {
        let response = put_contact(
            &app,
            &format!("/remote.php/dav/addressbooks/dev/contacts/{uid}.vcf"),
            &sample_contact(uid, fname),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    put_contact(
        &app,
        "/remote.php/dav/addressbooks/dev/contacts/friends.vcf",
        &group_contact("friends", "Friends", &["a", "b", "c"]),
    )
    .await;

    let query_body = r#"<?xml version="1.0"?>
<c:addressbook-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:carddav">
  <d:prop><d:getetag/><c:address-data/></d:prop>
</c:addressbook-query>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(query_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    for uid in ["a", "b", "c", "friends"] {
        assert!(body.contains(&format!("{uid}.vcf")));
    }

    let sync_body = r#"<?xml version="1.0"?>
<d:sync-collection xmlns:d="DAV:"><d:sync-token/><d:prop><d:getetag/></d:prop></d:sync-collection>"#;
    let req = Request::builder()
        .method("REPORT")
        .uri("/remote.php/dav/addressbooks/dev/contacts/")
        .body(Body::from(sync_body))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let body = body_string(response).await;
    for uid in ["a", "b", "c", "friends"] {
        assert!(body.contains(&format!("{uid}.vcf")));
    }
    assert!(body.contains("<d:sync-token>breadcloud-sync:4</d:sync-token>"));
}
