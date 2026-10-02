//! HTTP handlers for the four login-flow-v2 routes (see `lib.rs`'s
//! `owns_path`/`dispatch`). This is the only place in this crate that
//! reads request bodies, checks credentials (via
//! `breadcloud_webdav::AccountStore::authenticate`), or builds a response
//! -- `flow.rs` stays pure state-machine logic with no HTTP awareness.
//!
//! **CSRF, concretely.** The login page is not gated by any cookie or
//! session (this codebase has neither), so the classic "a hostile page
//! rides the victim's ambient session cookie" CSRF does not literally
//! apply here. What this crate defends against instead is a hostile page
//! blind-submitting a forged `POST /login/v2/flow/{token}` (via a hidden
//! auto-submitting form, or clickjacking through an invisible iframe) that
//! it could not have constructed correctly without first loading the real
//! form: the [`flow::FlowStore`] hands out a fixed CSRF token embedded as
//! a hidden field on `GET`, `try_grant` refuses any `POST` whose
//! `csrf_token` field does not match it, and every response from this
//! crate additionally sends `X-Frame-Options: DENY` /
//! `Content-Security-Policy: frame-ancestors 'none'` (see
//! `template.rs`) so the page cannot be framed at all. What this crate
//! does *not* defend against, because no CSRF mechanism can: an attacker
//! who initiates their own flow and phishes a victim into opening that
//! flow's real, legitimately-rendered `login` URL and typing their real
//! password into it -- that is a phishing risk inherent to login-flow-v2
//! itself (identical in real Nextcloud), mitigated only by users treating
//! "type your password into a browser tab an app just opened" with the
//! same caution as any login prompt, not by anything server-side.
//!
//! **The `app_password` this hands back is the account's real password.**
//! Per CLOUD-28's own scoping note, breadcloud does not implement Nextcloud's
//! separate, per-device app-password mechanism (a second random credential,
//! independently revocable, that never equals the login password) --
//! `flow::GrantedCredentials::app_password` is literally the plaintext the
//! user typed into the login form, captured only long enough to hand back
//! to the one poll that consumes it (see `flow.rs`'s `FlowStore::try_grant`/
//! `poll`). **This means anyone who intercepts a poll response, or
//! compromises the client that requested it, obtains full, standing
//! access to the real account -- the same blast radius as if the user had
//! typed their password directly into that client**, not the narrower,
//! independently-revocable blast radius a real app password would give.
//! This is an accepted, documented tradeoff matching breadcloud's existing
//! "keep it simple" stance elsewhere (no roles, no sharing, no per-device
//! session tracking -- see the workspace README's CLOUD-11 section), not
//! an oversight; see the workspace README's own "Login flow v2" section
//! for the same note in the user-facing docs.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use breadcloud_webdav::AccountStore;
use percent_encoding::percent_decode_str;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::flow::{self, FlowStore, FormViewError, GrantOutcome};
use crate::template;

/// Read and parse an `application/x-www-form-urlencoded` body into a
/// lookup map. A tiny local copy of the same technique
/// `breadcloud-webui::form::parse_urlencoded` uses, rather than a
/// cross-crate dependency on that (unrelated, `/ui/`-specific) crate for
/// one small utility -- see `breadcloud-server::combined`'s own local
/// `xml_escape` for the same "small shared utility, kept local" precedent
/// in this workspace.
fn parse_urlencoded(body: &[u8]) -> HashMap<String, String> {
    let text = String::from_utf8_lossy(body);
    let mut out = HashMap::new();
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(decode_field(key), decode_field(value));
    }
    out
}

fn decode_field(raw: &str) -> String {
    let spaced = raw.replace('+', " ");
    percent_decode_str(&spaced).decode_utf8_lossy().into_owned()
}

const MAX_FORM_BODY_BYTES: usize = 64 * 1024;

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The externally-visible base URL for this server, used to build the
/// absolute `login` URL and `poll.endpoint` the real protocol requires
/// (`init`'s JSON response embeds full URLs, not paths -- a client opens
/// `login` directly in a system browser and has no other base to resolve
/// a relative one against).
///
/// Reads `X-Forwarded-Proto`/`X-Forwarded-Host` when present (the shape a
/// fronting reverse proxy sets), falling back to the plain `Host` header
/// and `http`. **This is display data only, not a security decision**:
/// unlike, say, trusting a forwarded IP for access control, an attacker
/// who spoofs these headers on their own request only changes what URL
/// string comes back in *their own* response body -- it cannot affect
/// which flow gets created, which account it can be granted against, or
/// any other request's behavior, so the usual caution about blindly
/// trusting `X-Forwarded-*` does not carry the same risk here.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// `POST /index.php/login/v2` (real Nextcloud's own client-facing path,
/// matched exactly so the official Desktop/Android apps work unmodified).
/// No authentication, no request body. Rejects with `501 Not Implemented`
/// when no accounts are configured at all (open/dev mode, CLOUD-11): a
/// login flow exists to grant access to one of a real set of accounts, and
/// there is nothing to authenticate against in open mode, so pretending to
/// offer this flow there would be misleading rather than merely unused.
pub fn init(flows: &FlowStore, accounts: &AccountStore, headers: &HeaderMap) -> Response {
    if accounts.is_empty() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "login flow v2 requires at least one configured account (BREADCLOUD_ACCOUNT_<n>_*)",
        )
            .into_response();
    }

    let Some(initiated) = flows.initiate() else {
        // MAX_PENDING_FLOWS reached even after reaping expired entries --
        // see flow.rs's module doc. 503 (temporarily out of capacity), not
        // 500: nothing is broken, the server is just declining to grow
        // this particular piece of state any further right now.
        tracing::warn!(
            "login flow v2: at capacity ({} pending)",
            flow::MAX_PENDING_FLOWS
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many pending login flows, try again shortly",
        )
            .into_response();
    };

    let base = base_url(headers);
    let body = json!({
        "poll": {
            "token": initiated.poll_token,
            "endpoint": format!("{base}/login/v2/poll"),
        },
        "login": format!("{base}/login/v2/flow/{}", initiated.login_token),
    });
    json_response(StatusCode::OK, body)
}

/// `POST /login/v2/poll`, body `token=<poll_token>`
/// (`application/x-www-form-urlencoded`, matching the real client's own
/// request shape). `404` while pending, unknown, or expired -- a polling
/// client cannot tell those apart and does not need to (see
/// `flow::FlowStore::poll`'s doc comment) -- `200` with the granted
/// credentials on success, after which the token is consumed and can never
/// succeed again.
pub async fn poll(flows: &FlowStore, req: Request) -> Response {
    let base = base_url(req.headers());
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_FORM_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body too large or unreadable").into_response(),
    };
    let form = parse_urlencoded(&bytes);
    let Some(token) = form.get("token") else {
        return (StatusCode::BAD_REQUEST, "missing token").into_response();
    };

    match flows.poll(token) {
        Some(creds) => json_response(
            StatusCode::OK,
            json!({
                "server": base,
                "loginName": creds.login_name,
                "appPassword": creds.app_password,
            }),
        ),
        // Real Nextcloud's poll endpoint returns a bare 404 for "not
        // ready yet" / unknown / expired alike -- no JSON body to parse,
        // matching what the official clients already expect there.
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /login/v2/flow/{login_token}`: renders the login form for a real,
/// unexpired, still-pending flow; the "link no longer valid" page
/// otherwise.
pub fn flow_get(flows: &FlowStore, login_token: &str) -> Response {
    match flows.form_view(login_token) {
        Ok(csrf) => template::login_page(login_token, &csrf, None),
        Err(FormViewError::NotFound) => template::expired_or_unknown_page(),
    }
}

/// `POST /login/v2/flow/{login_token}`: the form submission. Body is
/// `application/x-www-form-urlencoded` with `username`, `password`, and
/// `csrf_token` fields (see `template::login_page`'s rendered form).
///
/// Credential checking happens *here*, via `AccountStore::authenticate`
/// (CLOUD-11's real account model -- the one credential path every
/// authentication surface in this server funnels through, see that
/// method's own doc comment) -- `flow.rs` never sees `AccountStore` or a
/// raw password outside of what this hands back on success.
///
/// The check is an argon2id verification, deliberately slow and CPU/memory
/// heavy, and this endpoint is reachable unauthenticated (anyone can start
/// their own flow and submit wrong passwords), so *how* it runs matters:
///
/// - **Never under the flow-store lock.** [`FlowStore::begin_grant`] does
///   the cheap checks under the lock and releases it; the verify runs with
///   no lock held; [`FlowStore::finish_grant`] re-takes the lock and
///   re-checks the flow is still pending before granting. A slow verify on
///   one flow therefore cannot delay `init`, `poll`, or any other flow.
/// - **Never on an async worker thread.** It runs via
///   `tokio::task::spawn_blocking`, so a pile of verifications cannot park
///   the runtime threads that serve every other route.
/// - **Bounded concurrency.** Moving the verify off the lock would
///   otherwise let an attacker start an unbounded number of them at once
///   (each allocates argon2's memory block and occupies a blocking-pool
///   thread, a pool that `tokio::fs`, used by the WebDAV routes, shares).
///   `verify_permits` caps how many run at once; the rest wait as cheap
///   async tasks holding no thread.
pub async fn flow_post(
    flows: &FlowStore,
    verify_permits: &Arc<Semaphore>,
    accounts: Arc<AccountStore>,
    login_token: &str,
    req: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(req.into_body(), MAX_FORM_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body too large or unreadable").into_response(),
    };
    let form = parse_urlencoded(&bytes);
    let username = form.get("username").cloned().unwrap_or_default();
    let password = form.get("password").cloned().unwrap_or_default();
    let submitted_csrf = form.get("csrf_token").cloned().unwrap_or_default();

    let outcome = match flows.begin_grant(login_token, &submitted_csrf) {
        Err(outcome) => outcome,
        Ok(ticket) => {
            let credentials =
                verify_credentials(verify_permits, accounts, username.clone(), password).await;
            flows.finish_grant(ticket, credentials)
        }
    };

    match outcome {
        GrantOutcome::Granted => {
            // The exact login_name a successful grant recorded is not
            // threaded back out of `finish_grant` (it only reports the
            // outcome, not the credentials -- those are for `poll` alone
            // to hand out, see `flow.rs`'s doc comment on why `poll`
            // consumes the flow). The account model treats a username as
            // its own canonical casing (see `AccountStore::authenticate`),
            // so the value the user submitted and the one that ends up in
            // `GrantedCredentials::login_name` always agree in practice;
            // this page shows exactly what was submitted, which the
            // successful `authenticate` call above already proved is that
            // same value.
            template::success_page(&username)
        }
        GrantOutcome::NotFound => template::expired_or_unknown_page(),
        GrantOutcome::CsrfMismatch => template::csrf_rejected_page(),
        GrantOutcome::InvalidCredentials => {
            match flows.form_view(login_token) {
                Ok(csrf) => template::login_page(
                    login_token,
                    &csrf,
                    Some("Incorrect username or password."),
                ),
                // The flow expired in the (tiny) window between the grant
                // attempt and this re-render -- fall back to the same "no
                // longer valid" page rather than a confusing partial form.
                Err(FormViewError::NotFound) => template::expired_or_unknown_page(),
            }
        }
    }
}

/// Run `AccountStore::authenticate` on a blocking thread, holding one of
/// `verify_permits` for the duration. Returns `Some((login_name,
/// app_password))` on success. Fails closed: if the blocking task panics or
/// is cancelled, that is reported as a failed check, never a success.
async fn verify_credentials(
    verify_permits: &Arc<Semaphore>,
    accounts: Arc<AccountStore>,
    username: String,
    password: String,
) -> Option<(String, String)> {
    let permit = verify_permits
        .clone()
        .acquire_owned()
        .await
        .expect("the verify semaphore is never closed");
    let joined = tokio::task::spawn_blocking(move || {
        // Held until the verify finishes, so the cap covers the real work.
        let _permit = permit;
        accounts
            .authenticate(&username, &password)
            .map(|login_name| (login_name, password))
    })
    .await;
    match joined {
        Ok(credentials) => credentials,
        Err(err) => {
            tracing::error!("login flow v2: credential check task failed: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use breadcloud_webdav::Account;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn form_request(body: String) -> Request {
        Request::builder()
            .method("POST")
            .body(Body::from(body))
            .unwrap()
    }

    /// The argon2 verify must not run on the async worker thread. On a
    /// single-threaded runtime that is directly observable and needs no
    /// wall-clock threshold: a ticker task that yields in a loop can only
    /// make progress while `flow_post` is suspended at an await point. If
    /// the verify ran inline (a synchronous call inside the async fn), the
    /// one runtime thread would be stuck inside it until it returned, and
    /// the ticker would never get a turn before `flow_post` completed.
    #[tokio::test(flavor = "current_thread")]
    async fn password_verification_does_not_block_the_async_runtime() {
        let flows = FlowStore::new();
        let accounts = Arc::new(AccountStore::new(vec![Account::new("alice", "right", 1)]));
        let permits = Arc::new(Semaphore::new(1));
        let flow = flows.initiate().unwrap();
        let csrf = flows.form_view(&flow.login_token).unwrap();

        let done = AtomicBool::new(false);
        let ticks = AtomicUsize::new(0);
        let post = async {
            let resp = flow_post(
                &flows,
                &permits,
                accounts.clone(),
                &flow.login_token,
                form_request(format!("username=alice&password=wrong&csrf_token={csrf}")),
            )
            .await;
            done.store(true, Ordering::SeqCst);
            resp
        };
        let ticker = async {
            while !done.load(Ordering::SeqCst) {
                ticks.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        };
        let (resp, ()) = tokio::join!(post, ticker);

        assert_eq!(resp.status(), StatusCode::OK); // form re-rendered
        assert!(
            ticks.load(Ordering::SeqCst) > 1,
            "the runtime made no progress while the password was verified"
        );
        // Wrong password: flow still pending, so the right one still works.
        let resp = flow_post(
            &flows,
            &permits,
            accounts,
            &flow.login_token,
            form_request(format!("username=alice&password=right&csrf_token={csrf}")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(flows.poll(&flow.poll_token).is_some());
    }
}
