//! Pre-WebDAV discovery endpoints real Nextcloud clients require before
//! they will even attempt to mount the DAV tree.
//!
//! mirall's "add account" flow, and every sync run after that, starts
//! with `GET .../status.php` (unauthenticated: this is how it tells a
//! plain webserver from a Nextcloud one before it has credentials to
//! send) and then `GET .../ocs/v1.php/cloud/capabilities` and
//! `GET .../ocs/v1.php/cloud/user` (authenticated) to learn the server
//! version and account details. Skipping any of these makes the real
//! client fail immediately with "Error connecting to server", before it
//! ever issues a single WebDAV request -- confirmed by running
//! `nextcloudcmd` against this server without them.
//!
//! The version/capability values here are static placeholders (not read
//! from any real Nextcloud install); they exist to satisfy the shape the
//! client's discovery code checks, not to claim feature parity with a
//! specific Nextcloud release.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

const STATUS_JSON: &str = r#"{"installed":true,"maintenance":false,"needsDbUpgrade":false,"version":"28.0.0.0","versionstring":"28.0.0","edition":"","productname":"breadcloud","extendedSupport":false}"#;

// CLOUD-8: the "notes" key is what the real Nextcloud Notes app's own
// capability probe looks for before it will call its API at all --
// confirmed against `nextcloud/notes`'s own `lib/AppInfo/Capabilities.php`
// (`Application::APP_ID => ['api_version' => ..., 'version' => ...,
// 'notes_path' => ...]`), not guessed. `api_version` matches the exact
// list that upstream's `Application::$API_VERSIONS` advertises
// (`breadcloud-notes` implements the plain `v1` routes those versions
// cover); `notes_path` names the fixed folder `breadcloud-notes` actually
// uses (see that crate's own `settings` handler for why it is fixed
// rather than configurable).
const CAPABILITIES_JSON: &str = r#"{"ocs":{"meta":{"status":"ok","statuscode":100,"message":"OK"},"data":{"version":{"major":28,"minor":0,"micro":0,"string":"28.0.0","edition":"","extended_support":false},"capabilities":{"core":{"pollinterval":60,"webdav-root":"remote.php/webdav"},"dav":{"chunking":"1.0"},"files":{"bigfileupload":true,"blacklisted_files":[],"chunking":"1.0","comments":false,"undelete":false,"versioning":true},"notes":{"api_version":["0.2","1.3","1.4"],"version":"4.7.0","notes_path":"Notes"}}}}}"#;

/// `GET /status.php`. Deliberately does not require authentication (the
/// auth middleware exempts it): real Nextcloud serves this endpoint
/// openly, since a client hitting "add account" doesn't have credentials
/// yet.
pub fn status() -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        STATUS_JSON,
    )
        .into_response()
}

/// `GET /ocs/v1.php/cloud/capabilities` (and the `v2.php` alias some
/// client versions use). Reports `dav.chunking: "1.0"` so the client
/// knows this server accepts NG chunked uploads (see
/// `crate::handlers::uploads`), and `files.versioning: true` now that
/// overwrites are captured into version history (CLOUD-4, see
/// `crate::versioning` and `crate::handlers::versions`). `undelete`
/// stays `false`: CLOUD-5 provides a minimal trash DAV endpoint using
/// opaque IDs, but it does not implement Nextcloud's trashbin protocol.
/// Advertising undelete would invite unsupported Nextcloud client probes.
pub fn capabilities() -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "application/json")],
        CAPABILITIES_JSON,
    )
        .into_response()
}

/// `GET /ocs/v1.php/cloud/user`. `id` is the real authenticated account
/// (CLOUD-11: `AppState.owner` in the combined server, falling back to
/// `crate::auth`'s single configured username for this crate's own
/// standalone use -- see `crate::dispatch`'s OCS-user branch).
pub fn user(username: &str) -> Response {
    let body = format!(
        r#"{{"ocs":{{"meta":{{"status":"ok","statuscode":100,"message":"OK"}},"data":{{"id":"{username}","display-name":"{username}","displayname":"{username}","email":null}}}}}}"#
    );
    (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
}
