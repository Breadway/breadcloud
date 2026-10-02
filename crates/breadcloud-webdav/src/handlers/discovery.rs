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
use breadcloud_storage::Quota;
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};

const STATUS_JSON: &str = r#"{"installed":true,"maintenance":false,"needsDbUpgrade":false,"version":"28.0.0.0","versionstring":"28.0.0","edition":"","productname":"breadcloud","extendedSupport":false}"#;

/// Which OCS API generation a request came in on. The only difference this
/// module cares about is the envelope's `statuscode`: v1 reports `100` for
/// success, v2 reports the plain HTTP status (`200`), and real Nextcloud
/// does the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OcsVersion {
    V1,
    V2,
}

impl OcsVersion {
    fn ok_code(self) -> u16 {
        match self {
            OcsVersion::V1 => 100,
            OcsVersion::V2 => 200,
        }
    }
}

/// A `GET` this module answers, resolved from the request path by
/// [`route`].
#[derive(Debug, PartialEq, Eq)]
pub enum OcsRoute {
    Capabilities,
    /// `cloud/user`: the authenticated account.
    CurrentUser,
    /// `cloud/users/<id>`: `id` is percent-decoded.
    User(String),
    Notifications,
    Activity,
    DirectEditing,
}

/// Resolve `raw_path` (still percent-encoded, prefix and all: a Nextcloud
/// install may live under a subpath) to an OCS endpoint this module
/// serves. The part after `/ocs/v{1,2}.php/` must match an endpoint
/// exactly (a trailing slash aside): a loose substring match would let
/// e.g. `cloud/users/<someone else>` be answered as `cloud/user`.
pub fn route(raw_path: &str) -> Option<(OcsVersion, OcsRoute)> {
    const V1: &str = "/ocs/v1.php/";
    const V2: &str = "/ocs/v2.php/";
    let (version, after) = match (raw_path.find(V1), raw_path.find(V2)) {
        (Some(i), _) => (OcsVersion::V1, &raw_path[i + V1.len()..]),
        (None, Some(i)) => (OcsVersion::V2, &raw_path[i + V2.len()..]),
        (None, None) => return None,
    };
    let rest = after.trim_end_matches('/');
    let route = match rest {
        "cloud/capabilities" => OcsRoute::Capabilities,
        "cloud/user" => OcsRoute::CurrentUser,
        "apps/notifications/api/v1/notifications" | "apps/notifications/api/v2/notifications" => {
            OcsRoute::Notifications
        }
        "apps/files/api/v1/directEditing" => OcsRoute::DirectEditing,
        _ => {
            if let Some(id) = rest.strip_prefix("cloud/users/") {
                if id.is_empty() || id.contains('/') {
                    return None;
                }
                OcsRoute::User(percent_decode_str(id).decode_utf8().ok()?.into_owned())
            } else if rest == "apps/activity/api/v2/activity"
                || rest.starts_with("apps/activity/api/v2/activity/")
            {
                // `.../activity/<filter>` is the same list, filtered; an
                // empty list is empty under every filter.
                OcsRoute::Activity
            } else {
                return None;
            }
        }
    };
    Some((version, route))
}

/// Wrap `data` in the OCS envelope, as JSON (which is all this module
/// serves, see [`route`]'s callers: real clients send `format=json`).
fn ocs(version: OcsVersion, data: Value) -> Response {
    envelope(StatusCode::OK, "ok", version.ok_code(), "OK", data)
}

fn envelope(http: StatusCode, status: &str, code: u16, message: &str, data: Value) -> Response {
    let body = json!({
        "ocs": {
            "meta": { "status": status, "statuscode": code, "message": message },
            "data": data,
        }
    });
    (
        http,
        [("Content-Type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

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

/// `GET /ocs/v{1,2}.php/cloud/capabilities`. Reports `dav.chunking: "1.0"`
/// so the client knows this server accepts NG chunked uploads (see
/// `crate::handlers::uploads`), and `files.versioning: true` now that
/// overwrites are captured into version history (CLOUD-4, see
/// `crate::versioning` and `crate::handlers::versions`). `undelete`
/// stays `false`: CLOUD-5 provides a minimal trash DAV endpoint using
/// opaque IDs, but it does not implement Nextcloud's trashbin protocol.
/// Advertising undelete would invite unsupported Nextcloud client probes.
///
/// Every capability group a client commonly checks is present and says
/// "off" explicitly (CLOUD-29), rather than being silently absent: clients
/// treat a missing key and an explicit `false` differently (a missing
/// `files_sharing.api_enabled` can read as "unknown, try it and see").
/// Absent-and-disabled groups:
///
/// - `files_sharing`: no sharing of any kind.
/// - `notifications`: empty `ocs-endpoints`, so a client that gates on
///   that list never polls; the endpoint itself still answers (see
///   [`notifications`]) for one that probes regardless.
/// - `activity`: empty `apiv2`, same idea.
/// - `theming`: not themed beyond the product name (see below).
///
/// `theming` deliberately carries only `name`, `slogan`, `color` and
/// `color-text`, and none of the `logo`/`background`/`favicon` URLs: a
/// client that sees a logo URL fetches it, and there is no theming
/// endpoint to serve one, so omitting those is the honest "no custom
/// artwork" signal. The `files.directEditing` key is likewise left out
/// (it would need a working editor list), with [`direct_editing`] there
/// for clients that probe the endpoint anyway.
pub fn capabilities(version: OcsVersion) -> Response {
    ocs(
        version,
        json!({
            "version": {
                "major": 28, "minor": 0, "micro": 0,
                "string": "28.0.0", "edition": "", "extended_support": false
            },
            "capabilities": {
                "core": { "pollinterval": 60, "webdav-root": "remote.php/webdav" },
                "dav": { "chunking": "1.0" },
                "files": {
                    "bigfileupload": true,
                    "blacklisted_files": [],
                    "chunking": "1.0",
                    "comments": false,
                    "undelete": false,
                    "versioning": true
                },
                "files_sharing": {
                    "api_enabled": false,
                    "resharing": false,
                    "group_sharing": false,
                    "public": { "enabled": false },
                    "user": { "send_mail": false },
                    "federation": { "outgoing": false, "incoming": false }
                },
                "notifications": { "ocs-endpoints": [] },
                "activity": { "apiv2": [] },
                // CLOUD-8: the "notes" key is what the real Nextcloud Notes
                // app's own capability probe looks for before it will call
                // its API at all -- confirmed against `nextcloud/notes`'s own
                // `lib/AppInfo/Capabilities.php` (`Application::APP_ID =>
                // ['api_version' => ..., 'version' => ..., 'notes_path' =>
                // ...]`), not guessed. `api_version` matches the exact list
                // that upstream's `Application::$API_VERSIONS` advertises
                // (`breadcloud-notes` implements the plain `v1` routes those
                // versions cover); `notes_path` names the fixed folder
                // `breadcloud-notes` actually uses (see that crate's own
                // `settings` handler for why it is fixed rather than
                // configurable).
                "notes": {
                    "api_version": ["0.2", "1.3", "1.4"],
                    "version": "4.7.0",
                    "notes_path": "Notes"
                },
                "theming": {
                    "name": "breadcloud",
                    "slogan": "",
                    "color": "#0082c9",
                    "color-text": "#ffffff"
                }
            }
        }),
    )
}

/// `GET /ocs/v{1,2}.php/cloud/user` and `cloud/users/<own id>`. `id` is the
/// real authenticated account (CLOUD-11: `AppState.owner` in the combined
/// server, falling back to `crate::auth`'s single configured username for
/// this crate's own standalone use -- see `crate::dispatch`).
///
/// `quota` is the same report PROPFIND's `quota-used-bytes` /
/// `quota-available-bytes` come from, present when it could be computed.
/// `quota.quota` is the configured limit in bytes, or `-3` (Nextcloud's
/// "unlimited") when this account has none. There is no email or group
/// model, so `email` is null and `groups` empty.
pub fn user(
    version: OcsVersion,
    username: &str,
    quota: Option<Quota>,
    limit_bytes: Option<u64>,
) -> Response {
    let mut data = json!({
        "id": username,
        "enabled": true,
        "display-name": username,
        "displayname": username,
        "email": null,
        "groups": [],
    });
    if let Some(q) = quota {
        let total = q.used_bytes.saturating_add(q.available_bytes);
        let relative = if total == 0 {
            0.0
        } else {
            (q.used_bytes as f64 / total as f64 * 10000.0).round() / 100.0
        };
        data["quota"] = json!({
            "free": q.available_bytes,
            "used": q.used_bytes,
            "total": total,
            "relative": relative,
            "quota": limit_bytes.map_or(json!(-3), |l| json!(l)),
        });
    }
    ocs(version, data)
}

/// `GET /ocs/v{1,2}.php/cloud/users/<id>` for an `id` other than the
/// authenticated account's own. Nextcloud answers a non-admin asking about
/// someone else with 403 (and not a 404, so that does not double as a
/// which-users-exist oracle: it is the same for existing and unknown ids).
/// There are no admin or subadmin roles here (CLOUD-11), so every such
/// request gets it.
pub fn user_forbidden(version: OcsVersion) -> Response {
    let (http, code) = match version {
        OcsVersion::V1 => (StatusCode::OK, 997),
        OcsVersion::V2 => (StatusCode::FORBIDDEN, 403),
    };
    envelope(http, "failure", code, "Unauthorised", Value::Null)
}

/// `GET /ocs/v2.php/apps/notifications/api/v{1,2}/notifications`: an empty
/// notification list. (The real endpoint answers `204 No Content` when
/// there is nothing; a 200 with an empty list is what a client's ordinary
/// list parsing handles without a special case.)
pub fn notifications(version: OcsVersion) -> Response {
    ocs(version, json!([]))
}

/// `GET /ocs/v2.php/apps/activity/api/v2/activity[/<filter>]`: an empty
/// activity feed.
pub fn activity(version: OcsVersion) -> Response {
    ocs(version, json!([]))
}

/// `GET /ocs/v2.php/apps/files/api/v1/directEditing`: no editors and no
/// creators, i.e. direct editing is unavailable.
pub fn direct_editing(version: OcsVersion) -> Response {
    ocs(version, json!({ "editors": {}, "creators": {} }))
}
