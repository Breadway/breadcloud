//! `OC-FileId`: a stable per-resource identifier Nextcloud clients expect
//! back from a chunked-upload finalize. Confirmed against the real
//! client: without an `OC-FileId` response header on the finalizing
//! MOVE, mirall's `PropagateUploadFileNG` aborts the whole upload with
//! "Missing File ID from server" even though the file assembled and
//! landed correctly -- it will not consider a chunked upload complete
//! without one.
//!
//! Real Nextcloud file IDs are database row identifiers that survive a
//! rename. This server has no such database yet (that is a separate,
//! later piece of the epic -- see the workspace README's roadmap), so
//! this derives a deterministic id from the resource's path instead: a
//! sync client just needs *a* stable, opaque identifier. It does,
//! however, care about the id's syntax: it parses this value into a
//! signed 64-bit integer internally, and an earlier hex-string version of
//! this id (`"bc0123..."`) produced "conversion to qint64 failed" and
//! then "The response provided by the server is missing data" in the
//! real client's log, confirmed against nextcloudcmd -- so this must be a
//! plain decimal number, small enough to fit `i64`.
//!
//! The one known gap this leaves versus a real Nextcloud id is that
//! renaming a file changes its derived id, where real Nextcloud would
//! keep it stable; nothing in this server's sync-client support depends
//! on file ids surviving a rename today.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use breadcloud_storage::DavPath;

pub fn for_path(path: &DavPath) -> String {
    // DefaultHasher::new() uses fixed, documented-stable keys (unlike
    // RandomState, which HashMap normally pairs it with), so this is
    // deterministic across requests and server restarts for the same
    // path. Masked to 53 bits so it fits comfortably within a positive
    // i64 (and, incidentally, within a JS-safe integer too) regardless
    // of how the client's own numeric parsing handles the value; `max(1)`
    // just keeps it non-zero, an all-zero hash being astronomically
    // unlikely but free to rule out.
    let mut hasher = DefaultHasher::new();
    path.to_request_path().hash(&mut hasher);
    let id = (hasher.finish() & ((1u64 << 53) - 1)).max(1);
    id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_yields_the_same_id() {
        let a = DavPath::parse("/docs/report.pdf").unwrap();
        let b = DavPath::parse("/docs/report.pdf").unwrap();
        assert_eq!(for_path(&a), for_path(&b));
    }

    #[test]
    fn different_paths_yield_different_ids() {
        let a = DavPath::parse("/docs/report.pdf").unwrap();
        let b = DavPath::parse("/docs/other.pdf").unwrap();
        assert_ne!(for_path(&a), for_path(&b));
    }

    #[test]
    fn id_is_a_plain_decimal_number_that_fits_i64() {
        // Real clients parse this into a signed 64-bit integer; anything
        // else (a hex string, a prefixed id) fails that parse silently
        // and the client then treats the file as inaccessible.
        for p in ["/a.txt", "/docs/report.pdf", "/", "/x/y/z"] {
            let id = for_path(&DavPath::parse(p).unwrap());
            assert!(
                id.chars().all(|c| c.is_ascii_digit()),
                "id {id} for {p} is not all-digit"
            );
            assert!(
                id.parse::<i64>().is_ok(),
                "id {id} for {p} does not fit i64"
            );
        }
    }
}
