//! Nextcloud Login Flow v2 (CLOUD-28): the poll-based, browser-driven
//! account setup the official Nextcloud Desktop and Android apps default
//! to. Without this, those clients cannot complete initial setup against
//! breadcloud at all (CLOUD-9 flagged exactly this for the official
//! Android app) -- this crate is what makes them work, unmodified,
//! against a breadcloud server.
//!
//! **The real protocol, verified against upstream, not guessed.** Routes,
//! JSON field names, status codes, the 1200-second (20 minute) token
//! lifetime, and the single-use-via-deletion semantics were all read
//! directly off `nextcloud/server`'s own source
//! (`core/Controller/ClientFlowLoginV2Controller.php`,
//! `core/Service/LoginFlowV2Service.php`, `core/Db/LoginFlowV2Mapper.php`)
//! and cross-checked against `docs.nextcloud.com`'s own Login Flow v2
//! page, the same "read the real implementation, not documentation
//! summaries alone" discipline CLOUD-6/7/8 each applied to their own
//! protocols. See `flow.rs`'s module doc for the token/expiry details and
//! `handlers.rs`'s for the CSRF/app-password tradeoffs this crate makes.
//!
//! **Four routes**, matching real Nextcloud's own path shapes so the
//! official clients work without modification:
//!
//! | Route | Auth | What |
//! |---|---|---|
//! | `POST /index.php/login/v2` | none | [`handlers::init`]: start a flow, return poll token + login URL |
//! | `POST /login/v2/poll` | poll token (body) | [`handlers::poll`]: 404 while pending, 200 + credentials once, then consumed |
//! | `GET /login/v2/flow/{token}` | login token (path) | [`handlers::flow_get`]: render the login form |
//! | `POST /login/v2/flow/{token}` | login token + CSRF + real credentials | [`handlers::flow_post`]: complete the flow |
//!
//! **One HTTP server, not two**, the same `owns_path`/`dispatch` pattern
//! `breadcloud-caldav`/`breadcloud-carddav`/`breadcloud-webui`/
//! `breadcloud-notes` already use: `breadcloud-server::combined` routes
//! anything this crate claims here from its one shared fallback. Unlike
//! every other protocol wired in there, though, this one runs *ahead of*
//! CLOUD-11's shared Basic-auth gate rather than behind it -- by design,
//! `init` and the browser-facing form are the mechanism by which a client
//! obtains credentials in the first place, so requiring them up front
//! would be circular. `breadcloud-server::combined::auth_gate` carries an
//! explicit exemption for these four paths, the same way it already
//! exempts `status.php` and `.well-known/{caldav,carddav}` for the same
//! "first request, before the client has anything to send" reason. See
//! that module's own doc comment.
//!
//! **Integrates with CLOUD-11's account model, not a parallel one.**
//! [`dispatch`] is handed the same `breadcloud_webdav::AccountStore`
//! `combined::auth_gate` already checks Basic-auth requests against;
//! `handlers::flow_post` calls its `authenticate` method (the same
//! username+password check `authenticate_request` itself now funnels
//! through) to verify what the browser form submits, so there is exactly
//! one place a password is ever checked against an account, regardless of
//! which surface (DAV Basic auth, the web UI's Basic auth, or this login
//! flow) a client authenticates through.

pub mod flow;
pub mod handlers;
mod template;

use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use breadcloud_webdav::AccountStore;
use percent_encoding::percent_decode_str;

pub use flow::FlowStore;

/// Shared state for this crate: just the one in-memory [`FlowStore`],
/// wrapped for cheap cloning into `breadcloud-server::combined`'s
/// `CombinedState` (which is itself `Clone`d into every request, the same
/// convention `breadcloud_caldav::AppState`/`breadcloud_carddav::AppState`
/// already follow for their own shared, non-per-account state).
#[derive(Clone)]
pub struct AppState {
    flows: Arc<FlowStore>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        AppState {
            flows: Arc::new(FlowStore::new()),
        }
    }
}

/// Whether `path` (the raw, still percent-encoded request path) belongs
/// to this crate. Mirrors `breadcloud_notes::owns_path`'s role in the
/// combined server's top-level dispatch. Matches the bare `/login/v2/flow`
/// prefix (not just `/login/v2/flow/<token>`) too, so a request with no
/// token segment still gets this crate's own 404 page instead of falling
/// through to an unrelated handler.
pub fn owns_path(path: &str) -> bool {
    path == "/index.php/login/v2" || path == "/login/v2/poll" || path.starts_with("/login/v2/flow")
}

/// The login-flow-v2 request dispatcher. `accounts` is the same
/// `breadcloud_webdav::AccountStore` `combined::auth_gate` checks Basic
/// auth against -- see this crate's module doc for why credential checking
/// funnels through the same store rather than a parallel one.
pub async fn dispatch(state: AppState, accounts: &AccountStore, req: Request) -> Response {
    let method = req.method().clone();
    let raw_path = req.uri().path().to_string();

    let decoded = match percent_decode_str(&raw_path).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response(),
    };

    if decoded == "/index.php/login/v2" {
        return match method.as_str() {
            "POST" => handlers::init(&state.flows, accounts, req.headers()),
            _ => method_not_allowed(),
        };
    }

    if decoded == "/login/v2/poll" {
        return match method.as_str() {
            "POST" => handlers::poll(&state.flows, req).await,
            _ => method_not_allowed(),
        };
    }

    if let Some(token) = decoded.strip_prefix("/login/v2/flow/") {
        if token.is_empty() {
            return (StatusCode::NOT_FOUND, "no such page").into_response();
        }
        return match method.as_str() {
            "GET" => handlers::flow_get(&state.flows, token),
            "POST" => handlers::flow_post(&state.flows, accounts, token, req).await,
            _ => method_not_allowed(),
        };
    }

    (StatusCode::NOT_FOUND, "no such page").into_response()
}

fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "unsupported method for this route",
    )
        .into_response()
}
