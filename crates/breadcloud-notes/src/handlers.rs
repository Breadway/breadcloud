//! Request handlers for each Notes OCS API route (see `crate::path`).
//!
//! Every handler that actually mutates a note's file content or removes
//! one goes through `breadcloud-storage`'s and `breadcloud-webdav`'s own
//! primitives (`Storage::write_bytes`, `versioning::snapshot_before_overwrite`,
//! `Storage::quota_gate`, `handlers::delete::handle`, `breadcloud_events`)
//! rather than re-implementing them -- the same pattern
//! `breadcloud-webui`'s `files` module already established for a
//! different wire format (multipart) over the same underlying writes: see
//! that module's own doc comment. This is what makes CLOUD-8's "a note
//! created/edited via this API is a real file" claim true rather than
//! asserted -- a note this API writes gets version history (CLOUD-4),
//! goes through trash on delete (CLOUD-5), and is quota-checked (CLOUD-11)
//! exactly like any other PUT, because it *is* one, at the storage layer.

use axum::body::Body;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_storage::{DavPath, StorageError};
use breadcloud_webdav::error::{insufficient_storage, storage_error_response};
use breadcloud_webdav::AppState;
use serde_json::{json, Value};

use crate::note::{self, Note};
use crate::path::notes_root;

/// `X-Notes-API-Versions` header value: every response from a real
/// Nextcloud Notes app carries this (`Helper::handleErrorResponse`,
/// confirmed against `nextcloud/notes`'s own controller code), listing
/// the API versions the server implements. Matches the `api_version`
/// array this crate also advertises in the OCS capabilities probe (see
/// `breadcloud-webdav`'s `discovery.rs`, updated by this issue).
const API_VERSIONS_HEADER: &str = "0.2, 1.3, 1.4";

fn with_api_versions_header(mut response: Response) -> Response {
    response.headers_mut().insert(
        "X-Notes-API-Versions",
        API_VERSIONS_HEADER.parse().expect("static header value"),
    );
    response
}

fn json_response(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// `{"errorType":"Exception"}`, real Nextcloud's own generic error body
/// shape (`Helper::createErrorResponse`) for every failure that isn't a
/// 412 conflict (which instead returns the current note, see
/// [`update`]).
fn error_response(status: StatusCode) -> Response {
    json_response(status, json!({"errorType": "Exception"}))
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND)
}

/// A `.md` file's storage error, mapped the way this API's own error
/// vocabulary (not WebDAV's) expects: `NotFound` -> 404 in the
/// `{"errorType":...}` shape rather than `storage_error_response`'s plain
/// text, `QuotaExceeded` -> 507 in that same shape. Anything else falls
/// back to `storage_error_response`'s WebDAV-flavored mapping, which is
/// still the right status code, just not JSON-wrapped -- acceptable for
/// errors a real client is not expected to parse structurally (a genuine
/// server bug, not a condition the Notes app's own error handling
/// branches on).
fn note_storage_error(err: StorageError) -> Response {
    match err {
        StorageError::NotFound => not_found(),
        StorageError::QuotaExceeded { .. } => error_response(insufficient_storage()),
        other => storage_error_response(other),
    }
}

fn parse_exclude(query: &str) -> Vec<String> {
    query_param(query, "exclude")
        .map(|v| {
            v.split(',')
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then_some(v)
    })
}

/// `GET /notes`: list every note, optionally filtered by `category`,
/// each rendered with `exclude`'s fields dropped. Sets the response
/// `ETag` to a hash of every note's own (id, etag) pair -- the "has
/// anything changed" shortcut CLOUD-8 asks for: a client that sends that
/// same value back as `If-None-Match` on its next poll gets a bare 304
/// instead of paying to re-fetch and re-serialize full note content it
/// already has, mirroring real Nextcloud's own `$response->setETag(...)`
/// on this same endpoint (`NotesApiController::index`).
pub async fn list(state: &AppState, headers: &HeaderMap, query: &str) -> Response {
    let category = query_param(query, "category").map(sanitize_query_str);
    let exclude = parse_exclude(query);

    let paths = match note::list_all(&state.storage).await {
        Ok(p) => p,
        Err(err) => return with_api_versions_header(note_storage_error(err)),
    };

    let mut notes = Vec::with_capacity(paths.len());
    for path in &paths {
        if let Some(want) = &category {
            if &note::category_of(path) != want {
                continue;
            }
        }
        match note::build(&state.storage, path).await {
            Ok(n) => notes.push(n),
            // A note that vanished between listing and reading (a
            // concurrent delete) is simply omitted, not a request
            // failure -- the next poll's listing will already reflect
            // its absence.
            Err(StorageError::NotFound) => continue,
            Err(err) => return with_api_versions_header(note_storage_error(err)),
        }
    }

    let collection_etag = collection_etag(&notes);
    if let Some(inm) = headers.get("If-None-Match").and_then(|v| v.to_str().ok()) {
        if inm.trim() == collection_etag {
            let response = Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header("ETag", collection_etag.as_str())
                .body(Body::empty())
                .unwrap();
            return with_api_versions_header(response);
        }
    }

    let body: Vec<Value> = notes.iter().map(|n| note::to_json(n, &exclude)).collect();
    let mut response = json_response(StatusCode::OK, Value::Array(body));
    response.headers_mut().insert(
        "ETag",
        collection_etag
            .parse()
            .expect("hex etag is valid header value"),
    );
    with_api_versions_header(response)
}

/// A stable hash of every listed note's `(id, etag)` pair, order-
/// independent (sorted first) so the collection etag only changes when
/// the actual membership or content changes, not when [`note::list_all`]'s
/// directory-walk order happens to differ between two otherwise-identical
/// requests.
fn collection_etag(notes: &[Note]) -> String {
    let mut pairs: Vec<String> = notes
        .iter()
        .map(|n| format!("{}:{}", n.id, n.etag))
        .collect();
    pairs.sort();
    let joined = pairs.join(",");
    format!("\"{}\"", &blake3::hash(joined.as_bytes()).to_hex()[..32])
}

/// `GET /notes/{id}`.
pub async fn get(state: &AppState, id: &str) -> Response {
    match resolve(state, id).await {
        Ok(Some((_, note))) => {
            let mut response = json_response(StatusCode::OK, note::to_json(&note, &[]));
            response.headers_mut().insert(
                "ETag",
                note.etag
                    .parse()
                    .expect("stored etag is a valid header value"),
            );
            with_api_versions_header(response)
        }
        Ok(None) => with_api_versions_header(not_found()),
        Err(err) => with_api_versions_header(note_storage_error(err)),
    }
}

async fn resolve(state: &AppState, id: &str) -> Result<Option<(DavPath, Note)>, StorageError> {
    let Some(path) = note::find_by_id(&state.storage, id).await? else {
        return Ok(None);
    };
    let built = note::build(&state.storage, &path).await?;
    Ok(Some((path, built)))
}

/// The fields a `POST /notes` or `PUT /notes/{id}` body may carry, per
/// the real API's own request shape (`NotesApiController::create`/
/// `::update` parameter lists). All optional: an absent field on update
/// means "leave this alone" (matching real Nextcloud's own `?string`/
/// `?bool` nullable-default parameters), and on create falls back to an
/// empty value.
#[derive(Default, serde::Deserialize)]
struct NoteInput {
    title: Option<String>,
    content: Option<String>,
    category: Option<String>,
    #[allow(dead_code)] // accepted, deliberately not persisted -- see `note` module docs.
    favorite: Option<bool>,
}

/// `POST /notes`: create a new note file under `Notes/` (or
/// `Notes/<category>/`), named from `title` (or the content's first line
/// when `title` is absent, matching real Nextcloud's `getTitleFromContent`
/// fallback the v0.2 `createAutoTitle` route uses and v1 clients still
/// commonly rely on for a title-less quick capture).
pub async fn create(state: &AppState, body: &[u8]) -> Response {
    let input: NoteInput = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return with_api_versions_header(error_response(StatusCode::BAD_REQUEST)),
    };
    let content = input.content.unwrap_or_default();
    let raw_title = input
        .title
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| content.clone());
    let title = note::sanitize_title(&raw_title);
    let category = note::sanitize_category(&input.category.unwrap_or_default());
    let dir = note::category_dir(&category);

    if let Err(resp) = note::ensure_notes_root(&state.storage)
        .await
        .map_err(note_storage_error)
    {
        return with_api_versions_header(resp);
    }
    if !category.is_empty() {
        if let Err(err) = ensure_category_dir(state, &dir).await {
            return with_api_versions_header(note_storage_error(err));
        }
    }

    // CLOUD-8 rework: serialize the whole "pick an unused filename, then
    // write it" sequence for this account, closing the check-then-write
    // race that let concurrent same-title creates silently destroy each
    // other's content (see `Storage::create_gate`'s own docs, and
    // CLOUD-11's `quota_gate` for the same pattern applied to the
    // check-then-write race around quota instead). Held across both
    // `generate_filename` and the write itself: dropping it in between
    // would just move the TOCTOU window rather than close it.
    let _create_guard = state.storage.create_gate().await;

    let filename = note::generate_filename(&state.storage, &dir, &title, None).await;
    let path = dir.child(&filename);

    match write_note_content(state, &path, content.as_bytes()).await {
        Ok(()) => match note::build(&state.storage, &path).await {
            Ok(n) => {
                with_api_versions_header(json_response(StatusCode::OK, note::to_json(&n, &[])))
            }
            Err(err) => with_api_versions_header(note_storage_error(err)),
        },
        Err(resp) => with_api_versions_header(*resp),
    }
}

/// Create every missing folder segment of `dir` under `Notes/` (MKCOL has
/// no recursive mode -- `Storage::mkcol` requires the immediate parent to
/// already exist -- so a multi-segment category like `"Work/Project"`
/// needs each level created in turn).
async fn ensure_category_dir(state: &AppState, dir: &DavPath) -> Result<(), StorageError> {
    let mut built = notes_root();
    for seg in dir.segments()[notes_root().segments().len()..].iter() {
        built = built.child(seg);
        match state.storage.mkcol(&built).await {
            Ok(()) | Err(StorageError::AlreadyExists) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// `PUT /notes/{id}`: update content, and/or rename (a `title` or
/// `category` change moves the underlying file, matching real
/// Nextcloud's `Note::setTitleCategory`). Honors `If-Match` the same way
/// CLOUD-3's WebDAV PUT does (`breadcloud_webdav::preconditions`) -- a
/// mismatch means someone else's change landed first -- but, matching
/// the real Notes API's own contract (`Helper::getNoteWithETagCheck`,
/// confirmed in `nextcloud/notes`'s source), the 412 body is the note's
/// *current* server-side state rather than a bare error, so a client can
/// offer the user a merge instead of just failing.
pub async fn update(state: &AppState, id: &str, headers: &HeaderMap, body: &[u8]) -> Response {
    let (path, current) = match resolve(state, id).await {
        Ok(Some(v)) => v,
        Ok(None) => return with_api_versions_header(not_found()),
        Err(err) => return with_api_versions_header(note_storage_error(err)),
    };

    if let Some(if_match) = headers.get("If-Match").and_then(|v| v.to_str().ok()) {
        if !etag_matches(if_match, &current.etag) {
            breadcloud_events::sync_conflict(
                state.owner_label(),
                &path.to_request_path(),
                breadcloud_events::ConflictReason::IfMatch,
            );
            return with_api_versions_header(json_response(
                StatusCode::PRECONDITION_FAILED,
                note::to_json(&current, &[]),
            ));
        }
    }

    let input: NoteInput = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return with_api_versions_header(error_response(StatusCode::BAD_REQUEST)),
    };

    let new_title = input
        .title
        .as_ref()
        .map(|t| note::sanitize_title(t))
        .unwrap_or_else(|| current.title.clone());
    let new_category = input
        .category
        .as_ref()
        .map(|c| note::sanitize_category(c))
        .unwrap_or_else(|| current.category.clone());

    let mut path = path;
    if new_title != current.title || new_category != current.category {
        let dir = note::category_dir(&new_category);
        if !new_category.is_empty() {
            if let Err(err) = ensure_category_dir(state, &dir).await {
                return with_api_versions_header(note_storage_error(err));
            }
        }
        let filename = note::generate_filename(&state.storage, &dir, &new_title, Some(id)).await;
        let new_path = dir.child(&filename);
        if new_path != path {
            if let Err(resp) = rename_note(state, &path, &new_path).await {
                return with_api_versions_header(*resp);
            }
            path = new_path;
        }
    }

    if let Some(content) = &input.content {
        if let Err(resp) = write_note_content(state, &path, content.as_bytes()).await {
            return with_api_versions_header(*resp);
        }
    }

    match note::build(&state.storage, &path).await {
        Ok(n) => with_api_versions_header(json_response(StatusCode::OK, note::to_json(&n, &[]))),
        Err(err) => with_api_versions_header(note_storage_error(err)),
    }
}

fn etag_matches(header_value: &str, etag: &str) -> bool {
    header_value.trim() == "*" || header_value.split(',').map(str::trim).any(|v| v == etag)
}

/// `DELETE /notes/{id}`: soft-delete via `breadcloud-webdav`'s own trash
/// path (CLOUD-5) -- reused directly (unlike create/update, DELETE has no
/// body to translate between wire formats, so there is nothing this
/// crate needs to do differently from a plain WebDAV DELETE of the same
/// path). Real Nextcloud's own `notes#destroy` returns `200` with an
/// empty JSON array body, not `204`; matched here for a real client that
/// checks the response shape rather than just the status code.
///
/// The note's `.attachments.<id>/` sibling, if any, is trashed too
/// (CLOUD-8 rework) -- as its own trash entry, via the exact same
/// `breadcloud_webdav::trash::move_to_trash` primitive, not bundled into
/// the note's entry. Bundling would need `breadcloud-storage::TrashStore`
/// to grow a multi-payload entry shape it does not have today (today one
/// entry is one moved filesystem path); giving the attachments directory
/// its own entry instead needs no change to that store at all, and still
/// gets it everything that matters: it is no longer left behind in the
/// live tree (the orphan/quota-leak bug), it is subject to the exact same
/// age/size retention caps as any other trashed content
/// (`TrashStore::enforce_caps`), and it is independently restorable from
/// the trashbin. The one thing it does *not* get is "restoring the note
/// also restores its attachments" in one step -- a real gap versus
/// bundling, accepted here because the alternative is a storage-format
/// change for a mobile-notes-attachments edge case, not because it was
/// overlooked.
pub async fn delete(state: &AppState, id: &str) -> Response {
    let path = match note::find_by_id(&state.storage, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return with_api_versions_header(not_found()),
        Err(err) => return with_api_versions_header(note_storage_error(err)),
    };

    let attachments_dir = path
        .parent()
        .map(|parent| parent.child(&note::attachment_dir_name(id)));

    let resp = breadcloud_webdav::handlers::delete::handle(state, &path).await;
    if !resp.status().is_success() {
        return with_api_versions_header(resp);
    }

    if let Some(dir) = attachments_dir {
        if state.storage.exists(&dir).await {
            if let Err(err) = breadcloud_webdav::trash::move_to_trash(state, &dir).await {
                tracing::warn!(
                    "note {id} deleted, but trashing its attachments directory {dir} failed: \
                     {status}",
                    status = breadcloud_webdav::error::storage_error_response(err).status()
                );
            }
        }
    }

    with_api_versions_header(json_response(StatusCode::OK, Value::Array(Vec::new())))
}

/// `GET /settings`: this server's fixed notes layout. Real Nextcloud lets
/// a user relocate the notes folder and choose a file suffix
/// (`SettingsService`); breadcloud does not support either today -- notes
/// always live at `Notes/` with a `.md` suffix (the issue's own framing),
/// so this reports that fixed pair rather than pretending either is a
/// real per-account setting.
pub async fn settings_get() -> Response {
    with_api_versions_header(json_response(
        StatusCode::OK,
        json!({"notesPath": "Notes", "fileSuffix": note::SUFFIX}),
    ))
}

/// `PUT /settings`: accepts a request only if it asks for the layout this
/// server already uses (a no-op confirmation); rejects an attempt to
/// actually relocate the notes folder or change the suffix with 400
/// rather than silently pretending to honor it. A real client that always
/// pushes its locally-configured settings back to the server on login
/// still works fine against the fixed defaults; one that specifically
/// tries to change them gets a clear "not supported" instead of a value
/// that quietly never took effect.
pub async fn settings_put(body: &[u8]) -> Response {
    #[derive(serde::Deserialize, Default)]
    struct SettingsInput {
        #[serde(rename = "notesPath")]
        notes_path: Option<String>,
        #[serde(rename = "fileSuffix")]
        file_suffix: Option<String>,
    }
    let input: SettingsInput = serde_json::from_slice(body).unwrap_or_default();
    let path_ok = input.notes_path.as_deref().is_none_or(|p| p == "Notes");
    let suffix_ok = input
        .file_suffix
        .as_deref()
        .is_none_or(|s| s == note::SUFFIX);
    if !path_ok || !suffix_ok {
        return with_api_versions_header(json_response(
            StatusCode::BAD_REQUEST,
            json!({
                "errorType": "Exception",
                "error": "this server always stores notes under \"Notes\" with a \".md\" suffix"
            }),
        ));
    }
    settings_get().await
}

/// `GET /attachment/{noteid}?path=<filename>`: stream back a file
/// previously uploaded via [`attachment_upload`], from
/// `Notes/<note's dir>/.attachments.<noteid>/<filename>` -- matches real
/// Nextcloud's own attachment folder convention
/// (`NoteUtil::getAttachmentFolderName`) so a `Notes/` tree this server
/// produces looks the same shape a real Nextcloud install's would.
pub async fn attachment_get(state: &AppState, note_id: &str, query: &str) -> Response {
    let Some(raw_name) = query_param(query, "path") else {
        return with_api_versions_header(error_response(StatusCode::BAD_REQUEST));
    };
    let Some((note_path, _)) = (match resolve(state, note_id).await {
        Ok(v) => v,
        Err(err) => return with_api_versions_header(note_storage_error(err)),
    }) else {
        return with_api_versions_header(not_found());
    };
    let Some(attachment_path) = attachment_path(&note_path, note_id, raw_name) else {
        return with_api_versions_header(error_response(StatusCode::BAD_REQUEST));
    };
    with_api_versions_header(
        breadcloud_webdav::handlers::get::handle(state, &attachment_path, false).await,
    )
}

/// Safety cap on an uploaded attachment, matching
/// `breadcloud-webui::files::MAX_UPLOAD_BYTES`'s own reasoning: notes
/// attachments (photos from a phone camera, mostly) are nowhere near the
/// multi-gigabyte territory `breadcloud-webdav`'s chunked-upload path
/// exists for.
const MAX_ATTACHMENT_BYTES: usize = 200 * 1024 * 1024;

/// `POST /attachment/{noteid}`: real Nextcloud sends this as
/// `multipart/form-data` with the file in a field named `file`
/// (`IRequest::getUploadedFile('file')`, confirmed in
/// `NotesApiController::uploadFile`) -- parsed here with axum's own
/// `Multipart` extractor, the same one `breadcloud-webui::files::upload`
/// already uses for its own (different-shaped) upload form.
pub async fn attachment_upload(state: &AppState, note_id: &str, req: Request) -> Response {
    let Some((note_path, _)) = (match resolve(state, note_id).await {
        Ok(v) => v,
        Err(err) => return with_api_versions_header(note_storage_error(err)),
    }) else {
        return with_api_versions_header(not_found());
    };

    let mut multipart = match axum::extract::Multipart::from_request(req, &()).await {
        Ok(m) => m,
        Err(err) => {
            return with_api_versions_header(
                (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
            )
        }
    };

    let mut file_name: Option<String> = None;
    let mut data: Vec<u8> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(err) => {
                return with_api_versions_header(
                    (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
                )
            }
        };
        if field.name() != Some("file") {
            continue;
        }
        file_name = field.file_name().map(str::to_string);
        data = match field.bytes().await {
            Ok(b) if b.len() > MAX_ATTACHMENT_BYTES => {
                return with_api_versions_header(
                    (StatusCode::PAYLOAD_TOO_LARGE, "attachment too large").into_response(),
                )
            }
            Ok(b) => b.to_vec(),
            Err(err) => {
                return with_api_versions_header(
                    (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
                )
            }
        };
    }
    let Some(file_name) = file_name.filter(|n| !n.is_empty()) else {
        return with_api_versions_header(error_response(StatusCode::BAD_REQUEST));
    };

    let sanitized = note::sanitize_attachment_name(&file_name);
    let dir = note_path
        .parent()
        .unwrap_or_else(notes_root)
        .child(&note::attachment_dir_name(note_id));
    if let Err(err) = ensure_category_dir(state, &dir).await {
        return with_api_versions_header(note_storage_error(err));
    }
    let dest = dir.child(&sanitized);
    match write_note_content(state, &dest, &data).await {
        Ok(()) => with_api_versions_header(json_response(
            StatusCode::OK,
            json!({"filename": format!("{}/{}", note::attachment_dir_name(note_id), sanitized)}),
        )),
        Err(resp) => with_api_versions_header(*resp),
    }
}

fn attachment_path(note_path: &DavPath, note_id: &str, raw_name: &str) -> Option<DavPath> {
    let sanitized = note::sanitize_attachment_name(raw_name);
    let dir = note_path
        .parent()?
        .child(&note::attachment_dir_name(note_id));
    Some(dir.child(&sanitized))
}

/// Write `data` to `path`, taking a version snapshot of whatever it
/// replaces first (CLOUD-4) and quota-gating the write (CLOUD-11) --
/// exactly the sequence `breadcloud_webdav::handlers::put::handle` runs
/// for a plain WebDAV `PUT`, factored out here because this crate's
/// request bodies arrive already parsed out of JSON/multipart rather than
/// as a raw request body `handlers::put::handle` could take directly
/// (the same reason `breadcloud-webui`'s own `files::upload` duplicates
/// this same short sequence instead of calling `handlers::put::handle`).
async fn write_note_content(
    state: &AppState,
    path: &DavPath,
    data: &[u8],
) -> Result<(), Box<Response>> {
    let _quota_guard = match state.quota_limit_bytes {
        Some(limit) => match state
            .storage
            .quota_gate(limit, path, data.len() as u64)
            .await
        {
            Ok(guard) => Some(guard),
            Err(err) => return Err(Box::new(note_storage_error(err))),
        },
        None => None,
    };

    // Rejected over-quota note writes must not create or evict versions.
    let pending_snapshot = breadcloud_webdav::versioning::snapshot_before_overwrite(state, path)
        .await
        .map_err(|err| Box::new(note_storage_error(err)))?;

    if let Err(err) = state.storage.write_bytes(path, data).await {
        breadcloud_webdav::versioning::discard_snapshot(state, path, pending_snapshot).await;
        return Err(Box::new(note_storage_error(err)));
    }
    breadcloud_webdav::versioning::finish_snapshot(state, path, pending_snapshot).await;

    breadcloud_events::file_synced(
        state.owner_label(),
        &path.to_request_path(),
        breadcloud_events::FileOp::Put,
        Some(data.len() as u64),
    );
    Ok(())
}

/// Move a note's file from `from` to `to` (a title/category-driven
/// rename), carrying its version history along -- the same two-step
/// `breadcloud_webdav::handlers::copy_move::handle` runs for a plain
/// WebDAV `MOVE`, called directly here since that handler is built around
/// a `Destination` header this crate has no equivalent of (the rename
/// target is computed from the note's new title/category, not read off a
/// header).
async fn rename_note(state: &AppState, from: &DavPath, to: &DavPath) -> Result<(), Box<Response>> {
    state
        .storage
        .move_to(from, to, false)
        .await
        .map_err(note_storage_error)?;
    if let Err(err) = state.versions.move_all(from, to).await {
        tracing::warn!("failed to carry version history from {from} to {to}: {err}");
    }
    rename_attachments_dir(state, from, to).await?;
    breadcloud_events::file_synced(
        state.owner_label(),
        &to.to_request_path(),
        breadcloud_events::FileOp::Move,
        None,
    );
    Ok(())
}

/// Carry a note's `.attachments.<id>/` sibling along with it on rename
/// (CLOUD-8 rework). A note's id is derived from its path
/// ([`breadcloud_webdav::fileid::for_path`], see `note` module docs), and
/// the attachment directory's name is in turn derived from the note's id
/// ([`note::attachment_dir_name`]) -- so a rename changes not just *where*
/// the attachments directory needs to live (alongside the note's new
/// path), but *what it needs to be named* (the note's new id). Relocating
/// the bytes to the new parent without renaming them to the new id would
/// leave the directory unaddressable through this API even though it is
/// no longer orphaned on disk, since [`attachment_path`] always derives
/// the directory name from the *current* id.
///
/// Reuses [`breadcloud_storage::Storage::move_to`] directly, the same
/// primitive the note's own move above just used, rather than hand-rolling
/// a second move implementation (CLOUD-5's own move primitive took five
/// rework rounds to get crash-safe; this is exactly that territory).
/// A no-op, not an error, when the note has no attachments directory.
async fn rename_attachments_dir(
    state: &AppState,
    from: &DavPath,
    to: &DavPath,
) -> Result<(), Box<Response>> {
    let (Some(from_parent), Some(to_parent)) = (from.parent(), to.parent()) else {
        return Ok(());
    };
    let old_id = breadcloud_webdav::fileid::for_path(from);
    let new_id = breadcloud_webdav::fileid::for_path(to);
    let old_dir = from_parent.child(&note::attachment_dir_name(&old_id));
    if !state.storage.exists(&old_dir).await {
        return Ok(());
    }
    let new_dir = to_parent.child(&note::attachment_dir_name(&new_id));
    state
        .storage
        .move_to(&old_dir, &new_dir, false)
        .await
        .map_err(note_storage_error)?;
    Ok(())
}

fn sanitize_query_str(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

/// Adapts [`create`]/[`update`] (which take an already-read body) to an
/// `axum::extract::Request`, buffering the body -- notes are small text
/// files, never the multi-gigabyte payloads `breadcloud-webdav`'s own PUT
/// path has to stream.
pub async fn read_json_body(req: Request) -> Result<Vec<u8>, Box<Response>> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(Box::new(with_api_versions_header(
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            )
                .into_response(),
        )));
    }

    axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024)
        .await
        .map(|b| b.to_vec())
        .map_err(|_| {
            Box::new(
                with_api_versions_header(error_response(StatusCode::BAD_REQUEST)).into_response(),
            )
        })
}
