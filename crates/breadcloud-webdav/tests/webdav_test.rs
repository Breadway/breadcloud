//! End-to-end tests against the real router (no server socket needed:
//! `tower::ServiceExt::oneshot` drives requests straight through the
//! `Service` axum builds).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_storage::{Storage, TrashStore, VersionStore};
use breadcloud_webdav::{router, AppState};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

fn test_app() -> (TempDir, TempDir, TempDir, TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let uploads_dir = TempDir::new().unwrap();
    let versions_dir = TempDir::new().unwrap();
    let trash_dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let trash = TrashStore::new(trash_dir.path());
    let state = AppState::new(storage, uploads, versions, trash);
    (dir, uploads_dir, versions_dir, trash_dir, router(state))
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn put_then_get_roundtrips_content() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put = Request::builder()
        .method("PUT")
        .uri("/hello.txt")
        .body(Body::from("hello world"))
        .unwrap();
    let response = app.clone().oneshot(put).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let get = Request::builder()
        .method("GET")
        .uri("/hello.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/plain"
    );
    assert!(response.headers().contains_key("etag"));
    let body = body_string(response).await;
    assert_eq!(body, "hello world");
}

#[tokio::test]
async fn put_overwrite_returns_no_content() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put1 = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("v1"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put1).await.unwrap().status(),
        StatusCode::CREATED
    );

    let put2 = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("v2, longer"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put2).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn put_with_missing_parent_is_conflict() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();
    let put = Request::builder()
        .method("PUT")
        .uri("/missing-dir/a.txt")
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(
        app.oneshot(put).await.unwrap().status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn get_missing_file_is_404() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();
    let get = Request::builder()
        .method("GET")
        .uri("/nope.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(get).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn mkcol_then_delete() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/photos")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    let mkcol_again = Request::builder()
        .method("MKCOL")
        .uri("/photos")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol_again).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );

    let delete = Request::builder()
        .method("DELETE")
        .uri("/photos")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(delete).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn propfind_depth_0_returns_only_the_resource_itself() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("hi"))
        .unwrap();
    app.clone().oneshot(put).await.unwrap();

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/")
        .header("Depth", "0")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
    assert!(xml.contains("<d:href>/</d:href>"));
    assert!(xml.contains("quota-used-bytes"));
}

#[tokio::test]
async fn propfind_depth_1_lists_children() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    for name in ["a.txt", "b.txt"] {
        let put = Request::builder()
            .method("PUT")
            .uri(format!("/{name}"))
            .body(Body::from("x"))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }
    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/sub")
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(mkcol).await.unwrap();

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/")
        .header("Depth", "1")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 4);
    assert!(xml.contains("<d:href>/sub/</d:href>"));
    assert!(xml.contains("<d:href>/a.txt</d:href>"));
    assert!(xml.contains("<d:href>/b.txt</d:href>"));
}

#[tokio::test]
async fn move_renames_resource() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("content"))
        .unwrap();
    app.clone().oneshot(put).await.unwrap();

    let mv = Request::builder()
        .method("MOVE")
        .uri("/a.txt")
        .header("Destination", "/b.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mv).await.unwrap().status(),
        StatusCode::CREATED
    );

    let get_old = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(get_old).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    let get_new = Request::builder()
        .method("GET")
        .uri("/b.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(get_new).await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn move_without_overwrite_onto_existing_is_412() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    for (name, content) in [("a.txt", "one"), ("b.txt", "two")] {
        let put = Request::builder()
            .method("PUT")
            .uri(format!("/{name}"))
            .body(Body::from(content))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }

    let mv = Request::builder()
        .method("MOVE")
        .uri("/a.txt")
        .header("Destination", "/b.txt")
        .header("Overwrite", "F")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(mv).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
}

#[tokio::test]
async fn copy_duplicates_resource_and_leaves_source_intact() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("content"))
        .unwrap();
    app.clone().oneshot(put).await.unwrap();

    let cp = Request::builder()
        .method("COPY")
        .uri("/a.txt")
        .header("Destination", "/copy.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(cp).await.unwrap().status(),
        StatusCode::CREATED
    );

    let get_source = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(get_source).await.unwrap().status(),
        StatusCode::OK
    );

    let get_copy = Request::builder()
        .method("GET")
        .uri("/copy.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(get_copy).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn copy_collection_into_own_subdirectory_is_409() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/a")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    let cp = Request::builder()
        .method("COPY")
        .uri("/a")
        .header("Destination", "/a/sub")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(cp).await.unwrap().status(),
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn options_advertises_dav_class_1_and_methods() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("DAV").unwrap(), "1");
    let allow = response.headers().get("Allow").unwrap().to_str().unwrap();
    assert!(allow.contains("PROPFIND"));
    assert!(allow.contains("MKCOL"));
}

#[tokio::test]
async fn chunked_finalize_with_stale_if_match_is_412_and_leaves_destination_untouched() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    // An existing destination file the client believes it holds the
    // current etag for, but does not (someone else changed it, or the
    // client's local cache is just stale).
    let put_dest = Request::builder()
        .method("PUT")
        .uri("/dest.txt")
        .body(Body::from("original content"))
        .unwrap();
    let response = app.clone().oneshot(put_dest).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let original_etag = response
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Drive the NG chunking v2 flow: MKCOL the transfer collection, PUT
    // one chunk into it.
    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/uploads/testuser/txn1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    let chunk_body = "replacement content";
    let put_chunk = Request::builder()
        .method("PUT")
        .uri("/remote.php/dav/uploads/testuser/txn1/1")
        .body(Body::from(chunk_body))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_chunk).await.unwrap().status(),
        StatusCode::CREATED
    );

    // Finalize with an If-Match that does not match the destination's
    // real current etag. This must 412 before assembling the chunks
    // onto /dest.txt, exactly like a plain PUT with the same stale
    // If-Match would.
    let finalize = Request::builder()
        .method("MOVE")
        .uri("/remote.php/dav/uploads/testuser/txn1/.file")
        .header("Destination", "/dest.txt")
        .header("If-Match", "\"totally-wrong-stale-etag\"")
        .header("OC-Total-Length", chunk_body.len().to_string())
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(finalize).await.unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

    // The destination must be entirely unaffected: same content, same
    // etag, no partial or replaced write.
    let get_dest = Request::builder()
        .method("GET")
        .uri("/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(get_dest).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("etag").unwrap(), &original_etag);
    let body = body_string(response).await;
    assert_eq!(body, "original content");
}

#[tokio::test]
async fn basic_auth_rejects_missing_and_wrong_credentials() {
    let dir = TempDir::new().unwrap();
    let uploads_dir = TempDir::new().unwrap();
    let versions_dir = TempDir::new().unwrap();
    let trash_dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let trash = TrashStore::new(trash_dir.path());
    let state = AppState::new(storage, uploads, versions, trash).with_auth("alice", "secret");
    let app = router(state);

    let no_auth = Request::builder()
        .method("GET")
        .uri("/")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(no_auth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let wrong_auth = Request::builder()
        .method("GET")
        .uri("/")
        .header("Authorization", "Basic d3Jvbmc6Y3JlZHM=") // wrong:creds
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(wrong_auth).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let right_auth = Request::builder()
        .method("GET")
        .uri("/")
        .header("Authorization", "Basic YWxpY2U6c2VjcmV0") // alice:secret
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(right_auth).await.unwrap();
    assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn first_put_creates_no_version_history() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put = Request::builder()
        .method("PUT")
        .uri("/a.txt")
        .body(Body::from("v1"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put).await.unwrap().status(),
        StatusCode::CREATED
    );

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);
}

#[tokio::test]
async fn overwriting_put_creates_a_browsable_and_fetchable_version() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    for content in ["first content", "second content, overwriting the first"] {
        let put = Request::builder()
            .method("PUT")
            .uri("/a.txt")
            .body(Body::from(content))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(propfind).await.unwrap();
    assert_eq!(response.status().as_u16(), 207);
    let xml = body_string(response).await;
    // Exactly one overwrite happened, so exactly one prior version is on
    // record (the second PUT's own content is live, not versioned).
    assert_eq!(xml.matches("<d:response>").count(), 1);
    assert!(xml.contains("<d:href>/remote.php/dav/versions/testuser/a.txt/"));

    let href_start = xml.find("<d:href>").unwrap() + "<d:href>".len();
    let href_end = xml[href_start..].find("</d:href>").unwrap() + href_start;
    let href = &xml[href_start..href_end];

    let get_version = Request::builder()
        .method("GET")
        .uri(href)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(get_version).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    // The retained version is the *previous* content, not the current one.
    assert_eq!(body, "first content");
}

#[tokio::test]
async fn version_history_is_capped_at_the_default_count() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    // One creation plus enough overwrites to push the count cap
    // (DEFAULT_MAX_VERSIONS_PER_FILE = 10): 12 overwrites means 12
    // candidate versions, so eviction must have happened.
    for i in 0..13 {
        let put = Request::builder()
            .method("PUT")
            .uri("/a.txt")
            .body(Body::from(format!("content-{i}")))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 10);
}

#[tokio::test]
async fn delete_purges_version_history() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    for content in ["v1", "v2"] {
        let put = Request::builder()
            .method("PUT")
            .uri("/a.txt")
            .body(Body::from(content))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }

    let delete = Request::builder()
        .method("DELETE")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(delete).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);
}

#[tokio::test]
async fn move_carries_version_history_to_the_new_path() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    for content in ["v1", "v2"] {
        let put = Request::builder()
            .method("PUT")
            .uri("/a.txt")
            .body(Body::from(content))
            .unwrap();
        app.clone().oneshot(put).await.unwrap();
    }

    let mv = Request::builder()
        .method("MOVE")
        .uri("/a.txt")
        .header("Destination", "/b.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mv).await.unwrap().status(),
        StatusCode::CREATED
    );

    let propfind_old = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let xml = body_string(app.clone().oneshot(propfind_old).await.unwrap()).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);

    let propfind_new = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/b.txt")
        .body(Body::empty())
        .unwrap();
    let xml = body_string(app.oneshot(propfind_new).await.unwrap()).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
}

#[tokio::test]
async fn chunked_finalize_overwrite_also_creates_a_version() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put_dest = Request::builder()
        .method("PUT")
        .uri("/dest.txt")
        .body(Body::from("original content"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_dest).await.unwrap().status(),
        StatusCode::CREATED
    );

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/uploads/testuser/txn1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    let chunk_body = "replacement content";
    let put_chunk = Request::builder()
        .method("PUT")
        .uri("/remote.php/dav/uploads/testuser/txn1/1")
        .body(Body::from(chunk_body))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_chunk).await.unwrap().status(),
        StatusCode::CREATED
    );

    let finalize = Request::builder()
        .method("MOVE")
        .uri("/remote.php/dav/uploads/testuser/txn1/.file")
        .header("Destination", "/dest.txt")
        .header("OC-Total-Length", chunk_body.len().to_string())
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(finalize).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
}

#[tokio::test]
async fn chunked_finalize_with_no_chunks_is_conflict_and_creates_no_version() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put_dest = Request::builder()
        .method("PUT")
        .uri("/dest.txt")
        .body(Body::from("original content"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_dest).await.unwrap().status(),
        StatusCode::CREATED
    );

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/uploads/testuser/txn-empty")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    // No chunks are ever PUT into the transfer collection -- finalizing
    // it should reject with 409 before touching `dest` at all.
    let finalize = Request::builder()
        .method("MOVE")
        .uri("/remote.php/dav/uploads/testuser/txn-empty/.file")
        .header("Destination", "/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(finalize).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let get_dest = Request::builder()
        .method("GET")
        .uri("/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get_dest).await.unwrap();
    assert_eq!(body_string(response).await, "original content");

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        0,
        "a rejected empty-chunk finalize must not create a version"
    );
}

#[tokio::test]
async fn retried_finalize_after_success_is_not_found_and_creates_no_extra_version() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let put_dest = Request::builder()
        .method("PUT")
        .uri("/dest.txt")
        .body(Body::from("original content"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_dest).await.unwrap().status(),
        StatusCode::CREATED
    );

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/remote.php/dav/uploads/testuser/txn-retry")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );

    let chunk_body = "replacement content";
    let put_chunk = Request::builder()
        .method("PUT")
        .uri("/remote.php/dav/uploads/testuser/txn-retry/1")
        .body(Body::from(chunk_body))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(put_chunk).await.unwrap().status(),
        StatusCode::CREATED
    );

    let finalize = || {
        Request::builder()
            .method("MOVE")
            .uri("/remote.php/dav/uploads/testuser/txn-retry/.file")
            .header("Destination", "/dest.txt")
            .header("OC-Total-Length", chunk_body.len().to_string())
            .body(Body::empty())
            .unwrap()
    };

    // First finalize genuinely succeeds and creates exactly one version.
    let response = app.clone().oneshot(finalize()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // A real client retries the identical finalize after a timeout, not
    // knowing the first attempt actually landed. The transfer collection
    // is already gone at this point, so this must 404, and must not
    // create a second, spurious version of unchanged content.
    let response = app.clone().oneshot(finalize()).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let propfind = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/dest.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        1,
        "a retried finalize against an already-completed transfer must not create an extra version"
    );
}

// --- CLOUD-5: soft-delete trash with a retention cap -----------------------

/// Extracts every `<d:href>` in a multistatus document, in document order.
fn extract_hrefs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<d:href>") {
        let after_tag = &rest[start + "<d:href>".len()..];
        let end = after_tag.find("</d:href>").unwrap();
        out.push(after_tag[..end].to_string());
        rest = &after_tag[end..];
    }
    out
}

async fn put(app: &axum::Router, uri: &str, content: &str) -> StatusCode {
    let req = Request::builder()
        .method("PUT")
        .uri(uri)
        .body(Body::from(content.to_string()))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn delete(app: &axum::Router, uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn propfind_trashbin(app: &axum::Router) -> (StatusCode, String) {
    let req = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/trashbin/testuser/")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    (status, body_string(response).await)
}

#[tokio::test]
async fn delete_does_not_remove_content_from_disk_it_moves_it_to_trash() {
    let (dir, _uploads_dir, _versions_dir, trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "hello").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    // Gone from the served tree...
    assert!(!dir.path().join("a.txt").exists());
    // ...but not actually erased: it is sitting somewhere under the trash
    // root, still recoverable, not on the floor of the served tree.
    let mut found = false;
    let mut stack = vec![trash_dir.path().to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == "payload" {
                found = true;
            } else if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    assert!(
        found,
        "trashed content must be findable under the trash root"
    );
}

#[tokio::test]
async fn deleted_item_is_listed_in_the_trashbin_with_original_location_and_deletion_time() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/docs")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        put(&app, "/docs/report.txt", "quarterly numbers").await,
        StatusCode::CREATED
    );

    assert_eq!(
        delete(&app, "/docs/report.txt").await,
        StatusCode::NO_CONTENT
    );

    let (status, xml) = propfind_trashbin(&app).await;
    assert_eq!(status.as_u16(), 207);
    assert_eq!(xml.matches("<d:response>").count(), 1);
    assert!(xml.contains(
        "<nc:trashbin-original-location>/docs/report.txt</nc:trashbin-original-location>"
    ));
    assert!(xml.contains("<nc:trashbin-deletion-time>"));
    assert!(xml.contains("<d:getcontentlength>17</d:getcontentlength>"));
}

#[tokio::test]
async fn restore_with_no_destination_returns_the_file_to_its_original_path() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(
        put(&app, "/a.txt", "original content").await,
        StatusCode::CREATED
    );
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    let restore = Request::builder()
        .method("MOVE")
        .uri(href)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(restore).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "original content");

    // The trash entry is fully consumed by the restore.
    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);
}

#[tokio::test]
async fn restore_onto_an_occupied_original_path_requires_overwrite_or_a_new_destination() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(
        put(&app, "/a.txt", "old content").await,
        StatusCode::CREATED
    );
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);
    // Someone (or something) recreated the path after the deletion.
    assert_eq!(
        put(&app, "/a.txt", "new content").await,
        StatusCode::CREATED
    );

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    let restore_no_overwrite = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(restore_no_overwrite)
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_FAILED
    );

    // A different destination works without needing Overwrite at all.
    let restore_elsewhere = Request::builder()
        .method("MOVE")
        .uri(&href)
        .header("Destination", "/restored-a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(restore_elsewhere)
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );

    let get = Request::builder()
        .method("GET")
        .uri("/restored-a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(body_string(response).await, "old content");

    // The occupied original path is untouched.
    let get_original = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(get_original).await.unwrap();
    assert_eq!(body_string(response).await, "new content");
}

#[tokio::test]
async fn restore_with_explicit_overwrite_replaces_the_occupied_destination() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(
        put(&app, "/a.txt", "old content").await,
        StatusCode::CREATED
    );
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);
    assert_eq!(
        put(&app, "/a.txt", "new content").await,
        StatusCode::CREATED
    );

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .header("Overwrite", "T")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restore).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(get).await.unwrap();
    assert_eq!(body_string(response).await, "old content");
}

#[tokio::test]
async fn delete_on_trashbin_entry_purges_it_permanently() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "x").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    let purge = Request::builder()
        .method("DELETE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(purge).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );

    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);

    // Purging an already-purged (or never-existing) id is a 404, not a
    // silent success, so a client can tell "gone" from "already gone".
    let purge_again = Request::builder()
        .method("DELETE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(purge_again).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // And it can no longer be restored either.
    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(restore).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_a_directory_trashes_and_restores_the_whole_subtree() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    let mkcol = Request::builder()
        .method("MKCOL")
        .uri("/photos")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mkcol).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        put(&app, "/photos/a.jpg", "jpeg-bytes").await,
        StatusCode::CREATED
    );

    assert_eq!(delete(&app, "/photos").await, StatusCode::NO_CONTENT);

    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
    assert!(xml.contains("<d:resourcetype><d:collection/></d:resourcetype>"));
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restore).await.unwrap().status(),
        StatusCode::CREATED
    );

    let get = Request::builder()
        .method("GET")
        .uri("/photos/a.jpg")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(get).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "jpeg-bytes");
}

#[tokio::test]
async fn restoring_a_deleted_file_brings_its_version_history_back() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "v1").await, StatusCode::CREATED);
    assert_eq!(put(&app, "/a.txt", "v2").await, StatusCode::NO_CONTENT);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    // Confirmed elsewhere (delete_purges_version_history) that the live
    // path's version surface is empty right after delete; the history is
    // not gone, though -- it travelled into the same trash entry, and
    // restoring should bring it back.
    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();
    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restore).await.unwrap().status(),
        StatusCode::CREATED
    );

    let propfind_versions = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind_versions).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        1,
        "the one prior version (v1) must have travelled back out of trash with the restored file"
    );
}

// Regression coverage for the round-3 CLOUD-5 fix: restore moves version
// history first and the payload last, and rolls the version-history move
// back if the payload move then fails, specifically so that a failure
// partway through never leaves the payload sitting on the live tree while
// the trash entry looks "occupied, retry with Overwrite: T" -- retrying
// that exact path used to delete the just-restored file and then find no
// payload left in trash to put back, destroying it for good. Both tests
// below force a genuine filesystem-level failure (permission denied, the
// same technique used to confirm the fix live) rather than only asserting
// on the happy path.

#[cfg(unix)]
#[tokio::test]
async fn restore_failing_on_version_history_move_leaves_trash_entry_fully_intact() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, _uploads_dir, versions_dir, _trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "v1").await, StatusCode::CREATED);
    assert_eq!(put(&app, "/a.txt", "v2").await, StatusCode::NO_CONTENT);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    // Force the version-history move (the first of restore's two moves)
    // to fail: block writes to the live version-history root, the same
    // as a real permissions problem or a read-only mount would.
    let mut perms = std::fs::metadata(versions_dir.path())
        .unwrap()
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(versions_dir.path(), perms.clone()).unwrap();

    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restore).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // Restore write access so the retry below (and test cleanup) can
    // proceed.
    perms.set_mode(0o755);
    std::fs::set_permissions(versions_dir.path(), perms).unwrap();

    // Nothing was restored: the live tree never saw the file...
    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(get).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    // ...and the trash entry is still fully there, ready for a clean
    // retry, not half-consumed.
    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);

    // A retry -- even a naive one sending Overwrite: T, as a client might
    // after seeing a server error -- must fully succeed and lose nothing.
    let retry = Request::builder()
        .method("MOVE")
        .uri(&href)
        .header("Overwrite", "T")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(retry).await.unwrap().status(),
        StatusCode::CREATED
    );

    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(body_string(response).await, "v2");

    let propfind_versions = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(propfind_versions).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        1,
        "version history must have survived the failed attempt and come back on retry"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restore_failing_on_payload_move_rolls_back_version_history_and_stays_retryable() {
    use std::os::unix::fs::PermissionsExt;

    let (dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "v1").await, StatusCode::CREATED);
    assert_eq!(put(&app, "/a.txt", "v2").await, StatusCode::NO_CONTENT);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);

    let (_status, xml) = propfind_trashbin(&app).await;
    let href = extract_hrefs(&xml).into_iter().next().unwrap();

    // Force the payload move (restore's second move, which runs only
    // after the version-history move already succeeded) to fail: block
    // writes to the live served tree so the rename/copy of the payload
    // back into place cannot land.
    let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(dir.path(), perms.clone()).unwrap();

    let restore = Request::builder()
        .method("MOVE")
        .uri(&href)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(restore).await.unwrap().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // Restore write access so the retry below (and test cleanup) can
    // proceed.
    perms.set_mode(0o755);
    std::fs::set_permissions(dir.path(), perms).unwrap();

    // This is exactly the sequence that used to destroy the file: before
    // this fix, the payload would already be sitting on the live tree at
    // this point, and a retry with Overwrite: T would delete it. Confirm
    // that cannot happen: nothing was restored.
    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(get).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // The trash entry must still be fully there: the payload never left
    // it, and the version history that did briefly move out to the live
    // tree must have been rolled back in, not left stranded there while
    // the entry itself looks half-consumed.
    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        1,
        "the trash entry must survive a failed restore intact, not be left half-consumed"
    );

    // A retry -- even with Overwrite: T, exactly what the old 412
    // response used to recommend -- must fully succeed and lose nothing.
    let retry = Request::builder()
        .method("MOVE")
        .uri(&href)
        .header("Overwrite", "T")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(retry).await.unwrap().status(),
        StatusCode::CREATED
    );

    let get = Request::builder()
        .method("GET")
        .uri("/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(get).await.unwrap();
    assert_eq!(body_string(response).await, "v2");

    let propfind_versions = Request::builder()
        .method("PROPFIND")
        .uri("/remote.php/dav/versions/testuser/a.txt")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(propfind_versions).await.unwrap();
    let xml = body_string(response).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        1,
        "version history must have been rolled back, then restored correctly on retry"
    );

    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 0);
}

#[tokio::test]
async fn move_of_a_live_file_does_not_go_through_trash() {
    let (_dir, _uploads_dir, _versions_dir, _trash_dir, app) = test_app();

    assert_eq!(put(&app, "/a.txt", "content").await, StatusCode::CREATED);
    let mv = Request::builder()
        .method("MOVE")
        .uri("/a.txt")
        .header("Destination", "/b.txt")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(mv).await.unwrap().status(),
        StatusCode::CREATED
    );

    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(
        xml.matches("<d:response>").count(),
        0,
        "a rename (MOVE) must never create a trash entry"
    );
}

#[tokio::test]
async fn trash_cap_enforcement_evicts_the_oldest_deletion_once_the_size_cap_is_exceeded() {
    // A dedicated app with a tiny size cap (rather than test_app()'s
    // defaults) so eviction is observable without writing gigabytes of
    // test data; the defaults themselves (DEFAULT_TRASH_MAX_AGE /
    // DEFAULT_TRASH_MAX_TOTAL_BYTES) are exercised directly by
    // breadcloud-storage's own unit tests.
    let dir = TempDir::new().unwrap();
    let uploads_dir = TempDir::new().unwrap();
    let versions_dir = TempDir::new().unwrap();
    let trash_dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let trash = TrashStore::with_limits(trash_dir.path(), std::time::Duration::from_secs(3600), 15);
    let state = AppState::new(storage, uploads, versions, trash);
    let app = router(state);

    assert_eq!(put(&app, "/a.txt", "0123456789").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "/a.txt").await, StatusCode::NO_CONTENT);
    assert_eq!(put(&app, "/b.txt", "0123456789").await, StatusCode::CREATED);
    assert_eq!(delete(&app, "/b.txt").await, StatusCode::NO_CONTENT);

    // The size cap (15 bytes) fits one 10-byte deletion but not two, so
    // enforcement (run after every delete) must have evicted the older
    // one once the second landed, keeping only the most recent deletion.
    let (_status, xml) = propfind_trashbin(&app).await;
    assert_eq!(xml.matches("<d:response>").count(), 1);
    assert!(xml.contains("<nc:trashbin-original-location>/b.txt</nc:trashbin-original-location>"));
}
