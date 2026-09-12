//! End-to-end tests against the real router (no server socket needed:
//! `tower::ServiceExt::oneshot` drives requests straight through the
//! `Service` axum builds).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_storage::Storage;
use breadcloud_webdav::{router, AppState};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

fn test_app() -> (TempDir, axum::Router) {
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let state = AppState::new(storage);
    (dir, router(state))
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn put_then_get_roundtrips_content() {
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();
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
    let (_dir, app) = test_app();
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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();

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
    let (_dir, app) = test_app();
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
async fn basic_auth_rejects_missing_and_wrong_credentials() {
    let dir = TempDir::new().unwrap();
    let storage = Storage::new(dir.path());
    let state = AppState::new(storage).with_auth("alice", "secret");
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
