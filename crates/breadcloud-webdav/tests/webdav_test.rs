//! End-to-end tests against the real router (no server socket needed:
//! `tower::ServiceExt::oneshot` drives requests straight through the
//! `Service` axum builds).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_storage::{Storage, VersionStore};
use breadcloud_webdav::{router, AppState};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

fn test_app() -> (TempDir, TempDir, TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let uploads_dir = TempDir::new().unwrap();
    let versions_dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let state = AppState::new(storage, uploads, versions);
    (dir, uploads_dir, versions_dir, router(state))
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn put_then_get_roundtrips_content() {
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();
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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();
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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();
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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let storage = Storage::new(dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let state = AppState::new(storage, uploads, versions).with_auth("alice", "secret");
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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
    let (_dir, _uploads_dir, _versions_dir, app) = test_app();

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
