//! End-to-end tests against `breadcloud_notes::dispatch` directly (no
//! router/socket needed, same as calling straight into
//! `breadcloud_webdav::dispatch` -- see that crate's own tests).
//!
//! These simulate the exact request sequences a real Nextcloud Notes
//! client issues, confirmed against `nextcloud/notes`'s own source
//! (`NotesApiController.php`, `Helper.php`) rather than guessed: a first
//! sync (`GET /notes`), a poll that finds nothing changed
//! (`If-None-Match`), create, an `If-Match`-guarded edit and its 412
//! conflict shape, a category-driven rename, delete-goes-to-trash, and
//! the settings/attachment sub-resources. See this crate's own README
//! entry for what was live-verified against the real server binary with
//! `curl` versus only exercised here.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use breadcloud_storage::{Storage, TrashStore, VersionStore};
use breadcloud_webdav::AppState;
use http_body_util::BodyExt;
use serde_json::Value;
use tempfile::TempDir;

const BASE: &str = "/index.php/apps/notes/api/v1";

struct TestDirs {
    _storage: TempDir,
    _uploads: TempDir,
    _versions: TempDir,
    _trash: TempDir,
}

fn test_state() -> (TestDirs, AppState) {
    let storage_dir = TempDir::new().unwrap();
    let uploads_dir = TempDir::new().unwrap();
    let versions_dir = TempDir::new().unwrap();
    let trash_dir = TempDir::new().unwrap();

    let storage = Storage::new(storage_dir.path());
    let uploads = Storage::new(uploads_dir.path());
    let versions = VersionStore::new(versions_dir.path());
    let trash = TrashStore::new(trash_dir.path());
    let state = AppState::new(storage, uploads, versions, trash).with_owner("dev");

    (
        TestDirs {
            _storage: storage_dir,
            _uploads: uploads_dir,
            _versions: versions_dir,
            _trash: trash_dir,
        },
        state,
    )
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn req(method: &str, path: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("{BASE}{path}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// A real client's very first sync: `GET /notes` against an account that
/// has never saved anything. Must not 404 -- `Notes/` is auto-created,
/// matching real Nextcloud's own `getAll($userId, true)`.
#[tokio::test]
async fn list_on_a_fresh_account_auto_creates_notes_and_returns_empty() {
    let (_dirs, state) = test_state();
    let response = breadcloud_notes::dispatch(state, req("GET", "/notes", Vec::new())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("etag"));
    assert!(response.headers().contains_key("x-notes-api-versions"));
    let body = body_json(response).await;
    assert_eq!(body, Value::Array(Vec::new()));
}

#[tokio::test]
async fn create_then_get_roundtrips_a_note() {
    let (_dirs, state) = test_state();

    let create_body = serde_json::json!({"title": "Shopping list", "content": "milk\neggs"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created_json = body_json(created).await;
    assert_eq!(created_json["title"], "Shopping list");
    assert_eq!(created_json["content"], "milk\neggs");
    assert_eq!(created_json["category"], "");
    assert_eq!(created_json["favorite"], false);
    assert_eq!(created_json["readonly"], false);
    let id = created_json["id"].as_str().unwrap().to_string();
    assert!(
        id.chars().all(|c| c.is_ascii_digit()),
        "id {id} is not numeric"
    );

    let fetched = breadcloud_notes::dispatch(
        state.clone(),
        req("GET", &format!("/notes/{id}"), Vec::new()),
    )
    .await;
    assert_eq!(fetched.status(), StatusCode::OK);
    let fetched_json = body_json(fetched).await;
    assert_eq!(fetched_json["id"], id);
    assert_eq!(fetched_json["content"], "milk\neggs");

    // The note is a real file under Notes/, byte-identical to what the
    // API wrote -- confirming this is a view over the same tree WebDAV
    // serves, not a second store (the issue's own core claim).
    let raw = state
        .storage
        .read_bytes(&breadcloud_storage::DavPath::parse("/Notes/Shopping list.md").unwrap())
        .await
        .unwrap();
    assert_eq!(raw, b"milk\neggs");
}

#[tokio::test]
async fn list_reflects_a_created_note_and_supports_exclude() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "todo", "content": "buy bread"});
    breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;

    let listed = breadcloud_notes::dispatch(state.clone(), req("GET", "/notes", Vec::new())).await;
    let listed_json = body_json(listed).await;
    let notes = listed_json.as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["title"], "todo");

    let excluded = breadcloud_notes::dispatch(
        state.clone(),
        req("GET", "/notes?exclude=content", Vec::new()),
    )
    .await;
    let excluded_json = body_json(excluded).await;
    assert!(excluded_json[0].get("content").is_none());
    assert!(excluded_json[0].get("title").is_some());
}

/// CLOUD-8's "has anything changed" shortcut: sending back the `ETag`
/// a prior `GET /notes` returned gets a bare 304, and nothing changing
/// keeps that etag stable across repeated polls.
#[tokio::test]
async fn list_if_none_match_short_circuits_to_304_when_unchanged() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "todo", "content": "x"});
    breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;

    let first = breadcloud_notes::dispatch(state.clone(), req("GET", "/notes", Vec::new())).await;
    let etag = first
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let mut second_req = req("GET", "/notes", Vec::new());
    second_req
        .headers_mut()
        .insert("If-None-Match", etag.parse().unwrap());
    let second = breadcloud_notes::dispatch(state.clone(), second_req).await;
    assert_eq!(second.status(), StatusCode::NOT_MODIFIED);

    // A genuine change moves the collection etag.
    let create_body2 = serde_json::json!({"title": "another", "content": "y"});
    breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body2).unwrap()),
    )
    .await;
    let third = breadcloud_notes::dispatch(state.clone(), req("GET", "/notes", Vec::new())).await;
    let new_etag = third.headers().get("etag").unwrap().to_str().unwrap();
    assert_ne!(etag, new_etag);
}

/// Real Nextcloud's own `If-Match` contract (`Helper::getNoteWithETagCheck`):
/// a mismatch is 412, and the body is the note's *current* server state,
/// not a bare error -- so a client can offer a merge.
#[tokio::test]
async fn update_with_stale_if_match_returns_412_with_current_note() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "todo", "content": "v1"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();

    let mut update_req = req(
        "PUT",
        &format!("/notes/{id}"),
        serde_json::to_vec(&serde_json::json!({"content": "v2"})).unwrap(),
    );
    update_req.headers_mut().insert(
        "If-Match",
        "\"stale-etag-that-never-matches\"".parse().unwrap(),
    );
    let response = breadcloud_notes::dispatch(state.clone(), update_req).await;
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    let conflict_json = body_json(response).await;
    assert_eq!(conflict_json["id"], id);
    assert_eq!(
        conflict_json["content"], "v1",
        "412 body must be the current server state"
    );

    // The stale write never landed.
    let fetched = breadcloud_notes::dispatch(
        state.clone(),
        req("GET", &format!("/notes/{id}"), Vec::new()),
    )
    .await;
    let fetched_json = body_json(fetched).await;
    assert_eq!(fetched_json["content"], "v1");
}

/// A correct `If-Match` succeeds, and the resulting write is versioned
/// (CLOUD-4) exactly like a plain WebDAV overwrite -- verified against
/// `breadcloud-storage::VersionStore` directly, not just asserted.
#[tokio::test]
async fn update_with_correct_if_match_succeeds_and_is_versioned() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "todo", "content": "v1"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();
    let etag = created_json["etag"].as_str().unwrap().to_string();

    let mut update_req = req(
        "PUT",
        &format!("/notes/{id}"),
        serde_json::to_vec(&serde_json::json!({"content": "v2"})).unwrap(),
    );
    update_req
        .headers_mut()
        .insert("If-Match", etag.parse().unwrap());
    let response = breadcloud_notes::dispatch(state.clone(), update_req).await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated_json = body_json(response).await;
    assert_eq!(updated_json["content"], "v2");

    let versions = state
        .versions
        .list(&breadcloud_storage::DavPath::parse("/Notes/todo.md").unwrap())
        .await
        .unwrap();
    assert_eq!(
        versions.len(),
        1,
        "the v1 content should be captured as a version"
    );
}

#[tokio::test]
async fn quota_rejected_note_overwrite_does_not_create_a_version() {
    let (_dirs, mut state) = test_state();
    state.quota_limit_bytes = Some(64);

    let created = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "POST",
            "/notes",
            serde_json::to_vec(&serde_json::json!({"title": "quota", "content": "v1"})).unwrap(),
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();

    let rejected = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "PUT",
            &format!("/notes/{id}"),
            serde_json::to_vec(&serde_json::json!({"content": "x".repeat(128)})).unwrap(),
        ),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::INSUFFICIENT_STORAGE);

    let versions = state
        .versions
        .list(&breadcloud_storage::DavPath::parse("/Notes/quota.md").unwrap())
        .await
        .unwrap();
    assert!(
        versions.is_empty(),
        "a rejected write must not create a version"
    );
}

/// A `category` change moves the underlying file into a subfolder, and
/// `category` in the response reflects it -- the folder-mapping CLOUD-8
/// asks for.
#[tokio::test]
async fn updating_category_moves_the_file_into_a_subfolder() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "recipe", "content": "bread"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();

    let update_body = serde_json::json!({"category": "Cooking"});
    let response = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "PUT",
            &format!("/notes/{id}"),
            serde_json::to_vec(&update_body).unwrap(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated_json = body_json(response).await;
    assert_eq!(updated_json["category"], "Cooking");
    let new_id = updated_json["id"].as_str().unwrap();

    let raw = state
        .storage
        .read_bytes(&breadcloud_storage::DavPath::parse("/Notes/Cooking/recipe.md").unwrap())
        .await
        .unwrap();
    assert_eq!(raw, b"bread");

    // Documented, known gap (see this crate's module docs): the derived
    // id changes across a rename, unlike real Nextcloud's database id.
    let refetched = breadcloud_notes::dispatch(
        state.clone(),
        req("GET", &format!("/notes/{new_id}"), Vec::new()),
    )
    .await;
    assert_eq!(refetched.status(), StatusCode::OK);
}

/// Real Nextcloud's own `destroy` response shape: `200` with an empty
/// JSON array, not a bare `204`. The note is soft-deleted (CLOUD-5), not
/// destroyed -- verified against `TrashStore` directly.
#[tokio::test]
async fn delete_returns_200_with_empty_array_and_goes_through_trash() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "gone soon", "content": "x"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();

    let response = breadcloud_notes::dispatch(
        state.clone(),
        req("DELETE", &format!("/notes/{id}"), Vec::new()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body, Value::Array(Vec::new()));

    let fetched = breadcloud_notes::dispatch(
        state.clone(),
        req("GET", &format!("/notes/{id}"), Vec::new()),
    )
    .await;
    assert_eq!(fetched.status(), StatusCode::NOT_FOUND);

    let trashed = state.trash.list().await.unwrap();
    assert_eq!(trashed.len(), 1);
    assert!(trashed[0]
        .original_path
        .to_request_path()
        .ends_with("gone soon.md"));
}

#[tokio::test]
async fn get_missing_note_is_404_with_error_shape() {
    let (_dirs, state) = test_state();
    let response =
        breadcloud_notes::dispatch(state, req("GET", "/notes/999999999", Vec::new())).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = body_json(response).await;
    assert_eq!(body["errorType"], "Exception");
}

#[tokio::test]
async fn settings_reports_the_fixed_layout() {
    let (_dirs, state) = test_state();
    let response = breadcloud_notes::dispatch(state, req("GET", "/settings", Vec::new())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["notesPath"], "Notes");
    assert_eq!(body["fileSuffix"], ".md");
}

#[tokio::test]
async fn settings_put_rejects_an_unsupported_relocation() {
    let (_dirs, state) = test_state();
    let response = breadcloud_notes::dispatch(
        state,
        req(
            "PUT",
            "/settings",
            serde_json::to_vec(&serde_json::json!({"notesPath": "SomewhereElse"})).unwrap(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A note whose file content is not valid UTF-8 (e.g. written by some
/// other tool directly under `Notes/`) surfaces as a readable error
/// rather than 500ing the whole request -- mirrors real Nextcloud's own
/// `error`/`errorType`/forced-`readonly` handling in `Note::getData`.
#[tokio::test]
async fn a_note_with_invalid_utf8_content_reports_an_error_instead_of_failing() {
    let (_dirs, state) = test_state();
    state
        .storage
        .mkcol(&breadcloud_storage::DavPath::parse("/Notes").unwrap())
        .await
        .unwrap();
    state
        .storage
        .write_bytes(
            &breadcloud_storage::DavPath::parse("/Notes/binary.md").unwrap(),
            &[0xff, 0xfe, 0x00, 0xff],
        )
        .await
        .unwrap();

    let response = breadcloud_notes::dispatch(state, req("GET", "/notes", Vec::new())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    let notes = body.as_array().unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["error"], true);
    assert_eq!(notes[0]["readonly"], true);
}

/// Build a `multipart/form-data` body with a single `file` field, matching
/// the shape `handlers::attachment_upload` parses (real Nextcloud Notes
/// clients send the same shape, see that handler's own docs).
fn multipart_file_body(boundary: &str, filename: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn upload_req(note_id: &str, boundary: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{BASE}/attachment/{note_id}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

/// CLOUD-8 review round 1 found this live: 20 concurrent `POST /notes`
/// with an identical title raced past `generate_filename`'s check-then-
/// write gap (it checks a candidate filename is free, then returns --
/// the actual write happens later and separately) and silently destroyed
/// several clients' content while every one of them got back a 200
/// claiming success. `Storage::create_gate` now serializes the whole
/// probe-then-write sequence per account; this fires the same flood
/// (matching the reviewer's live repro in spirit) and asserts every
/// client's distinct content survives under some filename, with no
/// silent overwrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_same_title_creates_lose_no_content() {
    let (_dirs, state) = test_state();
    const N: usize = 20;

    // Pre-create `Notes/` itself sequentially first: `mkcol`'s own
    // check-then-create has a separate, unreported race on the very first
    // note an account ever creates (20 requests against a brand new
    // account can all see "no Notes/ yet" and all call `create_dir`,
    // exactly one of which wins). That is not the bug under test here --
    // isolate the flood to the title-collision race this test exists to
    // catch by warming the folder up first, the same way a real account
    // would already have `Notes/` from any prior use.
    breadcloud_notes::note::ensure_notes_root(&state.storage)
        .await
        .unwrap();

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let state = state.clone();
        handles.push(tokio::spawn(async move {
            let create_body =
                serde_json::json!({"title": "todo", "content": format!("payload-{i}")});
            breadcloud_notes::dispatch(
                state,
                req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
            )
            .await
        }));
    }

    for handle in handles {
        let response = handle.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let listed = breadcloud_notes::dispatch(state.clone(), req("GET", "/notes", Vec::new())).await;
    let listed_json = body_json(listed).await;
    let notes = listed_json.as_array().unwrap();
    assert_eq!(
        notes.len(),
        N,
        "every concurrent create should have produced its own distinct file, none silently overwritten"
    );

    let mut contents: Vec<String> = notes
        .iter()
        .map(|n| n["content"].as_str().unwrap().to_string())
        .collect();
    contents.sort();
    let mut expected: Vec<String> = (0..N).map(|i| format!("payload-{i}")).collect();
    expected.sort();
    assert_eq!(
        contents, expected,
        "every distinct client payload must survive under some filename, none lost to an overwrite"
    );

    // Dedup suffixes were actually assigned, not just "some 20 files
    // happen to exist" -- every candidate name from "todo" through
    // "todo (20)" (in some order) should have been used exactly once.
    let mut titles: Vec<String> = notes
        .iter()
        .map(|n| n["title"].as_str().unwrap().to_string())
        .collect();
    titles.sort();
    titles.dedup();
    assert_eq!(
        titles.len(),
        N,
        "dedup suffixes must not collide with each other"
    );
}

/// CLOUD-8 review round 1 found this live: a category-changing `PUT`
/// moves the note file (`rename_note`) but left its `.attachments.<id>/`
/// sibling behind at the *old* path under the *old* id -- the attachment
/// then 404s under both the old and new id, since a note's id (and so its
/// attachment directory's name) is derived from its current path.
/// `rename_note` now carries the attachments directory along, renamed to
/// the note's *new* id, not just relocated under its old name.
#[tokio::test]
async fn attachment_survives_a_category_rename() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "recipe", "content": "bread"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let old_id = created_json["id"].as_str().unwrap().to_string();

    let boundary = "----breadcloud-test-boundary-1";
    let body = multipart_file_body(
        boundary,
        "photo.png",
        "application/octet-stream",
        b"fake-png-bytes",
    );
    let upload_resp =
        breadcloud_notes::dispatch(state.clone(), upload_req(&old_id, boundary, body)).await;
    assert_eq!(upload_resp.status(), StatusCode::OK);

    // Reachable at the old id before the rename.
    let get_before = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "GET",
            &format!("/attachment/{old_id}?path=photo.png"),
            Vec::new(),
        ),
    )
    .await;
    assert_eq!(get_before.status(), StatusCode::OK);

    let update_body = serde_json::json!({"category": "Cooking"});
    let response = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "PUT",
            &format!("/notes/{old_id}"),
            serde_json::to_vec(&update_body).unwrap(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated_json = body_json(response).await;
    let new_id = updated_json["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, old_id, "a category rename changes the derived id");

    // Reachable under the NEW id afterward, with correct content.
    let get_after = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "GET",
            &format!("/attachment/{new_id}?path=photo.png"),
            Vec::new(),
        ),
    )
    .await;
    assert_eq!(get_after.status(), StatusCode::OK);
    let bytes = get_after.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&bytes[..], b"fake-png-bytes");

    // No longer 404ing under the old id.
    let get_old_after = breadcloud_notes::dispatch(
        state.clone(),
        req(
            "GET",
            &format!("/attachment/{old_id}?path=photo.png"),
            Vec::new(),
        ),
    )
    .await;
    assert_eq!(get_old_after.status(), StatusCode::NOT_FOUND);

    // And not left behind as an orphan on disk under the old id either.
    let old_dir_gone = !state
        .storage
        .exists(
            &breadcloud_storage::DavPath::parse(&format!("/Notes/.attachments.{old_id}")).unwrap(),
        )
        .await;
    assert!(
        old_dir_gone,
        "old attachments directory must not be left behind"
    );
}

/// CLOUD-8 review round 1 found this live: deleting a note with an
/// attachment correctly trashed `note.md` via the real WebDAV trash path,
/// but left `.attachments.<id>/` behind forever in the live tree -- a
/// real, unbounded quota leak with no cleanup path. `delete` now trashes
/// the attachments directory too, as its own trash entry via the exact
/// same `move_to_trash` primitive, so it is no longer an orphan and is
/// subject to `TrashStore`'s own age/size retention caps like any other
/// trashed content, not exempt from them.
#[tokio::test]
async fn deleting_a_note_also_trashes_its_attachments_directory() {
    let (_dirs, state) = test_state();
    let create_body = serde_json::json!({"title": "gone soon", "content": "x"});
    let created = breadcloud_notes::dispatch(
        state.clone(),
        req("POST", "/notes", serde_json::to_vec(&create_body).unwrap()),
    )
    .await;
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_string();

    let boundary = "----breadcloud-test-boundary-2";
    let body = multipart_file_body(boundary, "note.txt", "text/plain", b"attachment bytes");
    let upload_resp =
        breadcloud_notes::dispatch(state.clone(), upload_req(&id, boundary, body)).await;
    assert_eq!(upload_resp.status(), StatusCode::OK);

    let attachments_dir =
        breadcloud_storage::DavPath::parse(&format!("/Notes/.attachments.{id}")).unwrap();
    assert!(state.storage.exists(&attachments_dir).await);

    let response = breadcloud_notes::dispatch(
        state.clone(),
        req("DELETE", &format!("/notes/{id}"), Vec::new()),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        !state.storage.exists(&attachments_dir).await,
        "attachments directory must not remain in the live tree after delete"
    );

    let trashed = state.trash.list().await.unwrap();
    assert_eq!(
        trashed.len(),
        2,
        "expected one trash entry for the note and one for its attachments directory"
    );
    let has_attachments_entry = trashed.iter().any(|e| {
        e.original_path
            .to_request_path()
            .ends_with(&format!(".attachments.{id}"))
    });
    assert!(
        has_attachments_entry,
        "the attachments directory should have its own trash entry, accounted for by the same \
         retention caps as any other trashed content, not silently orphaned outside trash"
    );
}

/// `owns_path` claims the documented prefix so `breadcloud-server::combined`
/// can route to it, and nothing outside it.
#[test]
fn owns_path_matches_the_real_client_base_url() {
    assert!(breadcloud_notes::owns_path(
        "/index.php/apps/notes/api/v1/notes"
    ));
    assert!(!breadcloud_notes::owns_path(
        "/remote.php/dav/files/dev/Notes/a.md"
    ));
}
