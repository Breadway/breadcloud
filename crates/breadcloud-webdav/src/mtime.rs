//! Support for the `X-OC-Mtime` header Nextcloud clients send on PUT and
//! on the finalizing MOVE of a chunked upload: a Unix timestamp for the
//! file's local modification time, which the server is expected to apply
//! to the stored file so that a later PROPFIND reports the client's own
//! mtime back rather than "whenever the upload happened".
//!
//! Without this, every sync client that preserves local mtimes across
//! machines would see the server's mtime differ from what it just wrote
//! and either loop re-uploading or spuriously flag a conflict on the very
//! file it just synced.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;

/// Parse the `X-OC-Mtime` header, if present and a valid Unix timestamp.
pub fn parse_oc_mtime(headers: &HeaderMap) -> Option<SystemTime> {
    let raw = headers.get("X-OC-Mtime")?.to_str().ok()?;
    let secs: i64 = raw.trim().parse().ok()?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Apply `mtime` to the file at `fs_path`. Best-effort: failure (the file
/// having vanished in a race, an unsupported filesystem) is logged and
/// otherwise ignored, since the upload itself already succeeded and the
/// mtime is a courtesy to the client's own conflict detection, not
/// something worth failing the request over.
///
/// Returns whether it succeeded, so the caller can decide whether to send
/// back the `X-OC-MTime: accepted` confirmation header real Nextcloud
/// clients check for: without it, mirall logs "Server does not support
/// X-OC-MTime" and falls back to trusting its own local mtime bookkeeping
/// instead of the server's -- confirmed against the real client, which
/// logs exactly that warning when this header is absent, even though the
/// mtime was in fact applied.
pub fn apply(fs_path: std::path::PathBuf, mtime: SystemTime) -> bool {
    match set_modified(&fs_path, mtime) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("failed to apply X-OC-Mtime to {}: {err}", fs_path.display());
            false
        }
    }
}

fn set_modified(fs_path: &Path, mtime: SystemTime) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().write(true).open(fs_path)?;
    file.set_modified(mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn parses_valid_unix_timestamp() {
        let mut headers = HeaderMap::new();
        headers.insert("X-OC-Mtime", HeaderValue::from_static("1700000000"));
        let mtime = parse_oc_mtime(&headers).unwrap();
        assert_eq!(
            mtime.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1700000000
        );
    }

    #[test]
    fn missing_header_is_none() {
        let headers = HeaderMap::new();
        assert!(parse_oc_mtime(&headers).is_none());
    }

    #[test]
    fn negative_or_garbage_is_none() {
        let mut headers = HeaderMap::new();
        headers.insert("X-OC-Mtime", HeaderValue::from_static("-5"));
        assert!(parse_oc_mtime(&headers).is_none());
        headers.insert("X-OC-Mtime", HeaderValue::from_static("not-a-number"));
        assert!(parse_oc_mtime(&headers).is_none());
    }

    #[test]
    fn applies_mtime_to_a_real_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"x").unwrap();

        let target = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        apply(path.clone(), target);

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.modified().unwrap(), target);
    }
}
