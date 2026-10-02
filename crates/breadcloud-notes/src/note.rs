//! The `Note` model: a JSON view over one markdown file already served by
//! `breadcloud-webdav`'s `Notes/` tree.
//!
//! Every field mirrors how the real Nextcloud Notes app derives it from a
//! plain file (`nextcloud/notes`'s `lib/Service/Note.php`, read directly
//! off GitHub rather than guessed), not an independent design of our own:
//!
//! - `id`: [`breadcloud_webdav::fileid::for_path`], the same deterministic
//!   per-path id chunked upload already relies on (CLOUD-3). Real
//!   Nextcloud uses the file's database row id, which survives a rename;
//!   this one does not (see that module's own doc comment for the
//!   existing, already-accepted gap). A rename here -- an update that
//!   changes `title` or `category` -- therefore changes `id` too, unlike
//!   real Nextcloud. Flagged explicitly rather than hidden: a real client
//!   that caches a note by id across a rename will refetch it as "new" the
//!   next time it lists notes, not silently lose it.
//! - `title`: the filename, minus its extension.
//! - `category`: the path of subfolders between `Notes/` and the file,
//!   `""` at the top level, `/`-joined for nested categories.
//! - `content`: the file's raw content, decoded as UTF-8.
//! - `modified`: the file's mtime as a Unix timestamp.
//! - `etag`: [`breadcloud_storage::Storage::etag`] -- the exact same
//!   content-derived etag WebDAV's own GET/PUT/PROPFIND already report
//!   for this path, so a note edited over WebDAV and one edited through
//!   this API always agree on whether it "changed" (CLOUD-3's conflict
//!   detection, reused directly -- see `crate::handlers`).
//! - `favorite` / `readonly`: always `false`. Real Nextcloud stores
//!   `favorite` as a tag external to the file (`OCA\Notes\Service\TagService`)
//!   and `readonly` from the filesystem's own write permission; breadcloud
//!   has neither a per-file tag store nor per-file permissions, so
//!   `favorite` is accepted on create/update (matching the real request
//!   shape) but not persisted, rather than adding a new sidecar-file
//!   mechanism for one cosmetic flag the issue does not ask for.
//! - `internalPath` / `shareTypes` / `isShared`: omitted. Real Nextcloud's
//!   values describe its sharing subsystem, which breadcloud does not
//!   have; a client that reads them for display can do without, and one
//!   that requires them is out of scope for this issue.

use breadcloud_storage::{DavPath, Storage, StorageError};
use serde_json::{json, Map, Value};

use crate::path::notes_root;

/// Filename extension every note this API creates gets. Matches the
/// issue's own framing and the workspace's existing assumption (see
/// CLOUD-8's investigation comment) that notes are markdown files;
/// unlike real Nextcloud's `fileSuffix` setting this is not configurable
/// (see `crate::handlers::settings`).
pub const SUFFIX: &str = ".md";

/// Longest sanitized title/category segment this module will generate,
/// matching `nextcloud/notes`'s own `NoteUtil::MAX_TITLE_LENGTH` so
/// filenames this server creates stay within the same bound a real
/// Nextcloud install enforces.
const MAX_TITLE_LENGTH: usize = 100;

/// A note's full JSON representation, before any `exclude` filtering is
/// applied (see [`to_json`]).
pub struct Note {
    pub id: String,
    pub title: String,
    pub content: String,
    pub category: String,
    pub modified: u64,
    pub etag: String,
    /// `Some(message)` when the file's bytes were not valid UTF-8 --
    /// mirrors real Nextcloud's own `error`/`errorType` handling in
    /// `Note::getData` (a content read failure surfaces in the response
    /// rather than 500ing the whole request).
    pub error: Option<String>,
}

/// Ensure `Notes/` exists, creating it if this is the very first note
/// operation for this account. Matches real Nextcloud's own
/// `NotesService::getAll($userId, true)` ("auto-create notes folder if
/// not exists") -- a client's very first `GET /notes` must not 404 just
/// because nothing has been saved there yet.
pub async fn ensure_notes_root(storage: &Storage) -> Result<(), StorageError> {
    match storage.mkcol(&notes_root()).await {
        Ok(()) | Err(StorageError::AlreadyExists) => Ok(()),
        Err(err) => Err(err),
    }
}

/// Every note (a `.md` file) under `Notes/`, at any depth. Skips anything
/// under a `.attachments.<id>/` folder (CLOUD-8's attachment sub-resource
/// stores uploaded files there, alongside the note they belong to --
/// never a note in its own right, see `crate::handlers::attachment`).
pub async fn list_all(storage: &Storage) -> Result<Vec<DavPath>, StorageError> {
    ensure_notes_root(storage).await?;
    let entries = storage.list_recursive(&notes_root()).await?;
    Ok(entries
        .into_iter()
        .filter(|e| {
            !e.metadata.is_dir()
                && e.path.name().is_some_and(|n| n.ends_with(SUFFIX))
                && !e
                    .path
                    .segments()
                    .iter()
                    .any(|seg| seg.starts_with(".attachments."))
        })
        .map(|e| e.path)
        .collect())
}

/// Find the note whose derived id is `id`. `O(n)` in the number of notes
/// (a linear scan, recomputing `fileid::for_path` for each): breadcloud
/// has no id-indexed database to look this up in directly (the same
/// reason `breadcloud_webdav::fileid` derives ids from paths at all rather
/// than assigning them), and CLOUD-8's own usage figures (~288K total)
/// mean this is at most a few dozen notes, not a scale where the scan is
/// a real cost.
pub async fn find_by_id(storage: &Storage, id: &str) -> Result<Option<DavPath>, StorageError> {
    for path in list_all(storage).await? {
        if breadcloud_webdav::fileid::for_path(&path) == id {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// The category a note at `path` (somewhere under `Notes/`) belongs to:
/// the `/`-joined subfolder segments between `Notes/` and the file itself,
/// `""` for a note directly in `Notes/`.
pub fn category_of(path: &DavPath) -> String {
    let root_len = notes_root().segments().len();
    let segs = path.segments();
    if segs.len() <= root_len + 1 {
        return String::new();
    }
    segs[root_len..segs.len() - 1].join("/")
}

/// A note's title: its filename, minus [`SUFFIX`].
pub fn title_of(path: &DavPath) -> String {
    path.name()
        .and_then(|n| n.strip_suffix(SUFFIX))
        .unwrap_or_else(|| path.name().unwrap_or_default())
        .to_string()
}

/// Build a [`Note`] by reading `path` out of `storage`. `path` must
/// already be known to exist and name a file (typically one
/// [`list_all`]/[`find_by_id`] returned).
pub async fn build(storage: &Storage, path: &DavPath) -> Result<Note, StorageError> {
    let metadata = storage.metadata(path).await?;
    let etag = storage.etag_for(path, &metadata).await?;
    let bytes = storage.read_bytes(path).await?;
    let (content, error) = match String::from_utf8(bytes) {
        Ok(s) => (s, None),
        Err(_) => (
            String::new(),
            Some("note content is not valid UTF-8".to_string()),
        ),
    };
    let modified = metadata
        .modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(Note {
        id: breadcloud_webdav::fileid::for_path(path),
        title: title_of(path),
        content,
        category: category_of(path),
        modified,
        etag,
        error,
    })
}

/// Render `note` as the JSON object a Notes client expects, dropping any
/// field named in `exclude` -- mirrors real Nextcloud's own `exclude`
/// query param (`Note::getData($exclude)`), which a client sends to skip
/// downloading full `content` on a routine list poll. `id`, `etag`,
/// `error`/`errorType` are never excludable, matching that same real
/// behavior (`Helper::getNoteData` always sets `etag` after the
/// exclude-filtered fields, and `Note::getData` always sets `error`/
/// `errorType` unconditionally).
pub fn to_json(note: &Note, exclude: &[String]) -> Value {
    let mut map = Map::new();
    let excluded = |field: &str| exclude.iter().any(|e| e == field);

    map.insert("id".to_string(), json!(note.id));
    if !excluded("title") {
        map.insert("title".to_string(), json!(note.title));
    }
    if !excluded("modified") {
        map.insert("modified".to_string(), json!(note.modified));
    }
    if !excluded("category") {
        map.insert("category".to_string(), json!(note.category));
    }
    if !excluded("favorite") {
        // Never persisted -- see this module's doc comment.
        map.insert("favorite".to_string(), json!(false));
    }
    if !excluded("readonly") {
        map.insert("readonly".to_string(), json!(note.error.is_some()));
    }
    map.insert("error".to_string(), json!(note.error.is_some()));
    map.insert(
        "errorType".to_string(),
        json!(note.error.clone().unwrap_or_default()),
    );
    if !excluded("content") {
        map.insert(
            "content".to_string(),
            json!(note.error.clone().unwrap_or_else(|| note.content.clone())),
        );
    }
    map.insert("etag".to_string(), json!(note.etag));

    Value::Object(map)
}

/// Strip characters illegal in a file/folder name on Windows (a superset
/// of what's illegal on Unix), which also rules out directory traversal
/// via an embedded slash -- ported from `nextcloud/notes`'s own
/// `NoteUtil::sanitisePath` so titles/categories this server accepts
/// produce filenames a real Nextcloud install would accept too.
fn sanitize_path_component(raw: &str) -> String {
    let stripped: String = raw
        .chars()
        .filter(|c| !['*', '|', '/', '\\', ':', '"', '<', '>', '?'].contains(c))
        .collect();
    // Real Nextcloud also strips leading dots/whitespace per line (so a
    // generated name can never be hidden or resolve to "." / ".."); this
    // server's titles/categories are always single-line by construction
    // (see `sanitize_title`, which takes only the content's first line),
    // so trimming the whole string covers the same case.
    stripped
        .trim_start_matches(['.', ' ', '\t'])
        .trim()
        .to_string()
}

/// A safe note title from `raw` (typically the client-supplied `title`
/// field, or the note's first line of content for a client that omits
/// `title`): sanitized, truncated to [`MAX_TITLE_LENGTH`], falling back to
/// "New note" if that leaves nothing. Mirrors
/// `NoteUtil::getSafeTitle`.
pub fn sanitize_title(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or("");
    let sanitized = sanitize_path_component(first_line);
    let truncated: String = sanitized.chars().take(MAX_TITLE_LENGTH).collect();
    if truncated.is_empty() {
        "New note".to_string()
    } else {
        truncated
    }
}

/// A safe category path from `raw`: each `/`-separated segment
/// sanitized independently, empty segments dropped, rejoined with `/`.
/// Mirrors `NoteUtil::normalizeCategoryPath`.
pub fn sanitize_category(raw: &str) -> String {
    raw.split('/')
        .map(sanitize_path_component)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// The folder a note in `category` (already sanitized) lives under,
/// relative to `Notes/`.
pub fn category_dir(category: &str) -> Result<DavPath, breadcloud_storage::path::InvalidPath> {
    let mut dir = notes_root();
    if !category.is_empty() {
        for seg in category.split('/') {
            dir = dir.child(seg)?;
        }
    }
    Ok(dir)
}

/// Choose a filename for a note titled `title` inside `dir`, avoiding a
/// collision with any file already there other than `keep_id` (the note
/// being renamed in place, if any -- passing its own id lets a title-only
/// edit that doesn't actually change the resulting filename skip the
/// dedup dance entirely, matching `NoteUtil::generateFileName`'s own
/// `$folder->get($filename)->getId() === $id` short-circuit). On a
/// collision, appends " (2)", " (3)", ... incrementing any such suffix
/// already on `title` rather than stacking a new one.
pub async fn generate_filename(
    storage: &Storage,
    dir: &DavPath,
    title: &str,
    keep_id: Option<&str>,
) -> Result<String, breadcloud_storage::path::InvalidPath> {
    let mut candidate_title = title.to_string();
    loop {
        let filename = format!("{candidate_title}{SUFFIX}");
        let candidate_path = dir.child(&filename)?;
        let taken = match storage.metadata(&candidate_path).await {
            Ok(_) => {
                let candidate_id = breadcloud_webdav::fileid::for_path(&candidate_path);
                keep_id != Some(candidate_id.as_str())
            }
            Err(_) => false,
        };
        if !taken {
            return Ok(filename);
        }
        candidate_title = increment_title_suffix(&candidate_title);
    }
}

/// " (2)" -> " (3)", or appends " (2)" if `title` has no such suffix yet.
/// Trims `title` first so the incremented suffix still fits within
/// [`MAX_TITLE_LENGTH`], matching `NoteUtil::generateFileName`'s own
/// truncate-before-append step.
fn increment_title_suffix(title: &str) -> String {
    let (base, next_n) = match title.rfind(" (") {
        Some(idx) if title.ends_with(')') => {
            let inner = &title[idx + 2..title.len() - 1];
            match inner.parse::<u32>() {
                Ok(n) => (&title[..idx], n + 1),
                Err(_) => (title, 2),
            }
        }
        _ => (title, 2),
    };
    let suffix = format!(" ({next_n})");
    let max_base = MAX_TITLE_LENGTH.saturating_sub(suffix.chars().count());
    let truncated_base: String = base.chars().take(max_base).collect();
    format!("{truncated_base}{suffix}")
}

/// The filesystem-relative folder an uploaded attachment for note `id`
/// lives in, alongside the note itself -- matches real Nextcloud's own
/// `NoteUtil::getAttachmentFolderName` naming convention exactly, so an
/// account's `Notes/` tree looks the same shape a real Nextcloud install's
/// would.
pub fn attachment_dir_name(id: &str) -> String {
    format!(".attachments.{id}")
}

/// Sanitize an attachment's client-supplied filename the same way a note
/// title is sanitized (no path separators, no leading dots), so an
/// attachment upload can never write outside its own attachment folder.
pub fn sanitize_attachment_name(raw: &str) -> String {
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let sanitized = sanitize_path_component(leaf);
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn category_of_top_level_note_is_empty() {
        let path = DavPath::parse("/Notes/shopping.md").unwrap();
        assert_eq!(category_of(&path), "");
    }

    #[test]
    fn category_of_nested_note_joins_segments() {
        let path = DavPath::parse("/Notes/Work/Project/todo.md").unwrap();
        assert_eq!(category_of(&path), "Work/Project");
    }

    #[test]
    fn title_of_strips_suffix() {
        let path = DavPath::parse("/Notes/shopping.md").unwrap();
        assert_eq!(title_of(&path), "shopping");
    }

    #[test]
    fn sanitize_title_strips_illegal_characters_and_takes_first_line() {
        assert_eq!(sanitize_title("a/b\\c:d\ne"), "abcd");
    }

    #[test]
    fn sanitize_title_falls_back_when_empty() {
        assert_eq!(sanitize_title(""), "New note");
        assert_eq!(sanitize_title("///"), "New note");
    }

    #[test]
    fn sanitize_title_truncates_to_max_length() {
        let long = "x".repeat(500);
        assert_eq!(sanitize_title(&long).chars().count(), MAX_TITLE_LENGTH);
    }

    #[test]
    fn sanitize_category_drops_empty_segments() {
        assert_eq!(sanitize_category("Work//Project/"), "Work/Project");
        assert_eq!(sanitize_category(""), "");
    }

    #[test]
    fn category_dir_builds_nested_path() {
        let dir = category_dir("Work/Project").unwrap();
        assert_eq!(dir.to_request_path(), "/Notes/Work/Project");
        assert_eq!(category_dir("").unwrap().to_request_path(), "/Notes");
    }

    #[test]
    fn increment_title_suffix_adds_then_bumps() {
        assert_eq!(increment_title_suffix("todo"), "todo (2)");
        assert_eq!(increment_title_suffix("todo (2)"), "todo (3)");
        assert_eq!(increment_title_suffix("todo (9)"), "todo (10)");
    }

    #[tokio::test]
    async fn generate_filename_avoids_existing_file() {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path());
        let notes_dir = notes_root();
        storage.mkcol(&notes_dir).await.unwrap();
        storage
            .write_bytes(&notes_dir.child("todo.md").unwrap(), b"one")
            .await
            .unwrap();

        let name = generate_filename(&storage, &notes_dir, "todo", None)
            .await
            .unwrap();
        assert_eq!(name, "todo (2).md");
    }

    #[tokio::test]
    async fn generate_filename_keeps_the_same_name_when_renaming_in_place() {
        let dir = TempDir::new().unwrap();
        let storage = Storage::new(dir.path());
        let notes_dir = notes_root();
        storage.mkcol(&notes_dir).await.unwrap();
        storage
            .write_bytes(&notes_dir.child("todo.md").unwrap(), b"one")
            .await
            .unwrap();
        let id = breadcloud_webdav::fileid::for_path(&notes_dir.child("todo.md").unwrap());

        let name = generate_filename(&storage, &notes_dir, "todo", Some(&id))
            .await
            .unwrap();
        assert_eq!(name, "todo.md");
    }
}
