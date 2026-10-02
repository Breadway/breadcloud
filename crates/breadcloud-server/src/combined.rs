//! Wires `breadcloud-webdav`, `breadcloud-caldav`, and `breadcloud-carddav`
//! into one axum app on one port, under one shared auth gate, instead of
//! running three HTTP servers for what is (as far as a client's TCP
//! connection is concerned) one breadcloud instance. See the workspace
//! README's "Sharing the HTTP server with CalDAV/CardDAV" section for why
//! this lives here rather than as shared code inside any protocol crate:
//! the composition itself -- "which protocol owns this path, and what
//! credentials gate all three of them" -- is a property of *this binary's*
//! configuration, not of any crate on its own.
//!
//! **Multi-user accounts (CLOUD-11).** Authentication now checks a real
//! set of independent accounts (`breadcloud_webdav::AccountStore`) instead
//! of a single fixed credential, and does it exactly once per request,
//! here, ahead of dispatching to any protocol: [`auth_gate`] resolves the
//! `Authorization` header to an authenticated username and stashes it in
//! the request's extensions as [`AuthenticatedUser`]. [`dispatch`] reads
//! that back, uses it to select which account's already-built
//! `breadcloud_webdav::AppState` (one per account, each rooted at its own
//! storage subtree -- see `breadcloud-server`'s `main.rs`) a WebDAV
//! request operates against, and -- just as importantly -- rejects
//! outright (403) any request whose URL names a *different* account than
//! the one that actually authenticated (`url_scoped_user`). Before this,
//! every handler that read a `<user>` segment out of a URL
//! (`/remote.php/dav/files/<user>/...`, `.../calendars/<user>/...`, and
//! so on) simply trusted it, which was safe only because there was ever
//! exactly one real account to begin with; CLOUD-11 is what makes that
//! segment adversarial input again and closes it back up.
//!
//! `breadcloud_caldav::AppState`/`breadcloud_carddav::AppState` stay
//! single, shared instances rather than one-per-account the way WebDAV's
//! does: `CalendarStore`/`AddressbookStore` already namespace every
//! operation by a `user` argument internally (see those types' own module
//! docs in `breadcloud-storage`), so passing the *validated* authenticated
//! username through to them is already real isolation, with no need to
//! also give each account a separate `CalendarStore`/`AddressbookStore`
//! instance the way each account needs its own `Storage`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use breadcloud_webdav::AccountStore;

/// The authenticated username for the current request, stashed by
/// [`auth_gate`] and read back by [`dispatch`]. Local to this module: each
/// protocol crate's own `dispatch` takes the identity it needs as an
/// explicit parameter instead (see `breadcloud_caldav::dispatch`'s
/// `authenticated_user` argument), since [`dispatch`] below is the one
/// place that has both this and the raw request path in hand together, to
/// run the URL-vs-authenticated-account check before anything else sees
/// the request.
#[derive(Clone)]
struct AuthenticatedUser(String);

#[derive(Clone)]
struct CombinedState {
    accounts: Arc<AccountStore>,
    /// One already-built `AppState` per account, each rooted at that
    /// account's own storage subtree (see `main.rs`). Keyed by username;
    /// looked up once per request in [`dispatch`] using the identity
    /// [`auth_gate`] already established.
    webdav_by_user: Arc<HashMap<String, breadcloud_webdav::AppState>>,
    caldav: breadcloud_caldav::AppState,
    carddav: breadcloud_carddav::AppState,
    /// The account every request operates as when `accounts` is empty
    /// (no accounts configured at all -- the open, unauthenticated
    /// local-dev mode this server has always supported). Ignored
    /// whenever `accounts` is non-empty, since then every request must
    /// authenticate as one of them.
    default_user: String,
}

/// Build the combined app. Deliberately does not call any crate's own
/// `router()` (each of which layers its own independent auth middleware,
/// meant for that crate's standalone use and its own test suite): doing
/// that here would check credentials multiple times per request and,
/// per the CLOUD-6 implementation notes, composing routers that each only
/// use a top-level `.fallback()` does not work cleanly with axum's
/// routing. Instead, this calls each crate's plain `dispatch` function
/// directly from one shared fallback, gated by one shared auth middleware.
pub fn router(
    accounts: AccountStore,
    webdav_by_user: HashMap<String, breadcloud_webdav::AppState>,
    caldav: breadcloud_caldav::AppState,
    carddav: breadcloud_carddav::AppState,
    default_user: impl Into<String>,
) -> Router {
    let state = CombinedState {
        accounts: Arc::new(accounts),
        webdav_by_user: Arc::new(webdav_by_user),
        caldav,
        carddav,
        default_user: default_user.into(),
    };
    Router::new()
        .fallback(dispatch)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_gate))
        // axum applies its own 2 MiB default body-size limit to any
        // request read through a `DefaultBodyLimit`-aware extractor,
        // silently overriding a handler's own larger cap unless this is
        // raised to match. `breadcloud_webui::files::upload`'s
        // `Multipart::from_request` is the *only* such extractor
        // anywhere in this workspace -- every other handler
        // (`breadcloud_webdav::handlers::put`, this crate's own
        // `mkdir`/`delete` form-body reads, and so on) reads its body via
        // a plain `axum::body::to_bytes(req.into_body(), <its own cap>)`
        // call on the raw `Request`, which this layer has no effect on
        // at all -- so raising it here, once, for the whole combined
        // router is equivalent to raising it for exactly the one route
        // that needs it, not a blanket loosening of every route's body
        // size handling.
        .layer(DefaultBodyLimit::max(breadcloud_webui::MAX_UPLOAD_BYTES))
}

/// Path prefixes where the very next segment names an account (Nextcloud's
/// own URL convention, carried through by CLOUD-3/6/7). See
/// [`url_scoped_user`].
const USER_SCOPED_PREFIXES: &[&str] = &[
    "/remote.php/dav/files/",
    "/remote.php/dav/uploads/",
    "/remote.php/dav/versions/",
    "/remote.php/dav/trashbin/",
    "/remote.php/dav/principals/",
    "/remote.php/dav/calendars/",
    "/remote.php/dav/addressbooks/",
];

/// If `path` starts with one of [`USER_SCOPED_PREFIXES`], the account
/// segment immediately after it (up to the next `/` or the end of the
/// path). `None` for a path with no such segment (a bare
/// `/remote.php/webdav` mount, a non-Nextcloud client's mount straight at
/// the server root, or `.well-known/*`) -- those have nothing to validate
/// against, since they never name an account in the URL at all.
fn url_scoped_user(path: &str) -> Option<&str> {
    for prefix in USER_SCOPED_PREFIXES {
        if let Some(rest) = path.strip_prefix(prefix) {
            let end = rest.find('/').unwrap_or(rest.len());
            let user = &rest[..end];
            if !user.is_empty() {
                return Some(user);
            }
        }
    }
    None
}

/// True only for the one request shape that means "a browser navigated to
/// the bare server root", never for anything a WebDAV/CalDAV/CardDAV
/// client sends there. Two conditions, both required:
///
/// - Method is exactly GET. PROPFIND, OPTIONS, PUT, DELETE, MKCOL, and
///   every other verb this dispatcher's bare-root fallthrough answers are
///   untouched by this check no matter what headers they carry --
///   `dispatch` only ever calls this for the one fallthrough branch below,
///   and even there the method guard here means non-GET requests always
///   return `false` and keep going exactly as before.
/// - The request's `Accept` header names `text/html`. This is the same
///   "is this a browser, not a CLI/sync client" signal used elsewhere on
///   the web: a real browser navigation always sends
///   `Accept: text/html,...`, while every WebDAV/CalDAV/CardDAV client
///   this codebase has been tested against (rclone, DAVx5, vdirsyncer,
///   Cyberduck, the `breadcloud-webdav` test suite's own bare `curl`-style
///   requests) either omits `Accept` entirely or sends `Accept: */*` --
///   consistent with `breadcloud_webdav::handlers::get`'s own doc comment
///   that real sync/file-manager clients never issue a bare GET on a
///   collection at all (they discover via PROPFIND), so this header check
///   is a second, belt-and-suspenders guard on top of an already-narrow
///   method+path match, not the only thing standing between this and a
///   real client's request.
fn is_browser_root_navigation(method: &Method, path: &str, headers: &HeaderMap) -> bool {
    if method != Method::GET || path != "/" {
        return false;
    }
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

async fn dispatch(State(state): State<CombinedState>, req: Request) -> Response {
    let path = req.uri().path().to_string();

    let authed_user = match req.extensions().get::<AuthenticatedUser>() {
        Some(user) => user.0.clone(),
        None if state.accounts.is_empty() => state.default_user.clone(),
        None => return unauthorized(),
    };

    // CLOUD-11: the URL's own account segment (if it has one) is
    // client-supplied and must never be trusted on its own to pick whose
    // storage a request operates on -- `auth_gate` has already
    // established who *actually* authenticated. Reject outright rather
    // than silently substituting one for the other when they disagree:
    // a real Nextcloud-family server does the same (403, not a quiet
    // redirect to the caller's own data).
    if let Some(url_user) = url_scoped_user(&path) {
        if url_user != authed_user {
            return forbidden();
        }
    }

    let Some(webdav_state) = state.webdav_by_user.get(&authed_user).cloned() else {
        // Unreachable in practice: `auth_gate` only ever produces a
        // username that is either a real account (checked against
        // `state.accounts`, which `main.rs` builds `webdav_by_user` from
        // 1:1) or `default_user` (which `main.rs` always adds an entry
        // for too). Fails loud rather than panicking a request thread if
        // that invariant is ever broken by a future change.
        tracing::error!("no storage configured for authenticated account \"{authed_user}\"");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "no storage configured for this account",
        )
            .into_response();
    };

    if breadcloud_notes::owns_path(&path) {
        // CLOUD-8: the Notes OCS API. Runs against `authed_user`'s own
        // already-resolved `webdav_state` exactly like the web UI branch
        // below -- a note is a real file in that account's own `Notes/`
        // tree, so it needs that account's own `Storage`/`VersionStore`,
        // not a separate store of its own. Checked before
        // `breadcloud_webui::owns_path` since neither claims the other's
        // URL space, so the order between them doesn't matter; this one
        // is listed first only because CLOUD-8 was implemented after
        // CLOUD-12.
        return breadcloud_notes::dispatch(webdav_state, req).await;
    }

    if breadcloud_webui::owns_path(&path) {
        // CLOUD-12: the minimal web UI. Runs against `authed_user`'s own
        // already-resolved `webdav_state` and the shared `caldav` state,
        // exactly the identity `auth_gate` already established above --
        // there is no `<user>` segment in this URL space at all for the
        // `url_scoped_user` check above to have anything to validate (see
        // `breadcloud_webui`'s own doc comment).
        return breadcloud_webui::dispatch(webdav_state, state.caldav.clone(), &authed_user, req)
            .await;
    }

    if breadcloud_caldav::owns_path(&path) {
        // A PROPFIND on the principal resource is the second hop of the
        // discovery chain both DAVx5 and iOS's Contacts account type walk
        // (bare root -> current-user-principal -> principal ->
        // addressbook-home-set), the exact same hop `calendar-home-set`
        // already rides today for CalDAV. `breadcloud-caldav` answers
        // this PROPFIND (it owns `/remote.php/dav/principals`) but knows
        // nothing about addressbooks, so this splices
        // `card:addressbook-home-set` onto that same response
        // additively, the same technique used below for
        // `current-user-principal` on the bare-root WebDAV response --
        // see this module's and `breadcloud_carddav`'s own doc comments
        // for why the principal-level splice, not a second bare-root
        // splice, is the right place: this mirrors exactly how a
        // calendar is already discovered, which is the literal ask
        // ("discover the addressbook the same way they now discover
        // calendars").
        let addressbook_home_href = (req.method().as_str() == "PROPFIND")
            .then(|| principal_path_user(&path))
            .flatten()
            .map(|user| breadcloud_carddav::addressbook_home_href(&user));
        let response =
            breadcloud_caldav::dispatch(state.caldav.clone(), Some(authed_user.as_str()), req)
                .await;
        return match addressbook_home_href {
            Some(href) => {
                add_href_prop(
                    response,
                    "card:addressbook-home-set xmlns:card=\"urn:ietf:params:xml:ns:carddav\"",
                    "card:addressbook-home-set",
                    &href,
                )
                .await
            }
            None => response,
        };
    }

    if breadcloud_carddav::owns_path(&path) {
        return breadcloud_carddav::dispatch(state.carddav.clone(), req).await;
    }

    // CLOUD-24: a plain browser navigating to the bare server root gets
    // sent to the web UI's file browser instead of falling through to
    // `breadcloud_webdav`'s own bare directory-listing GET fallback (see
    // `handlers::get::directory_listing`'s doc comment) -- that listing
    // exists for `curl`/debugging, not as the thing a person expects to
    // land on when they type the server's address into a browser.
    // Checked ahead of (and does not touch) every WebDAV verb this same
    // path already answers: PROPFIND (the CLOUD-6 discovery splice right
    // below), OPTIONS, PUT, DELETE, and so on all keep falling through to
    // `breadcloud_webdav::dispatch` exactly as before, since
    // `is_browser_root_navigation` only ever returns `true` for GET.
    if is_browser_root_navigation(req.method(), &path, req.headers()) {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", "/ui/files/")
            .body(Body::empty())
            .unwrap();
    }

    // A PROPFIND at the bare server root is exactly the request DAVx5's
    // and iOS's "just enter a server address" account setup send first
    // (CLOUD-6's review comment, reproduced with vdirsyncer): they probe
    // "/" for `current-user-principal` and only retry via
    // `.well-known/caldav`/`.well-known/carddav` when that probe *errors*
    // -- not when it merely succeeds without the property they wanted.
    // `owns_path` (both crates') correctly doesn't claim the bare root
    // (it isn't CalDAV's or CardDAV's, WebDAV answers it too), so that
    // request falls through to `breadcloud_webdav::dispatch` below
    // unchanged; here we additionally splice CalDAV's
    // `current-user-principal` into that same response so the one real
    // request a client actually sends carries what it needs, without ever
    // touching `breadcloud-webdav`'s own (separately tested) PROPFIND
    // output. Built from `authed_user` directly (CLOUD-11), not from a
    // fixed `AppState.auth` the way this used to read
    // `breadcloud_caldav::principal_href` -- the bare root has no `<user>`
    // segment of its own to validate, so this is the one place the
    // authenticated identity alone decides whose principal gets
    // advertised.
    let want_principal = req.method().as_str() == "PROPFIND" && path == "/";
    let principal_href =
        want_principal.then(|| breadcloud_caldav::principal_href_for(&authed_user));
    let response = breadcloud_webdav::dispatch(webdav_state, req).await;

    match principal_href {
        Some(href) => {
            add_href_prop(
                response,
                "d:current-user-principal",
                "d:current-user-principal",
                &href,
            )
            .await
        }
        None => response,
    }
}

/// If `path` is `/remote.php/dav/principals/<user>/` (or without the
/// trailing slash) for exactly one user segment, the `<user>` it names --
/// the same shape `breadcloud_caldav::path::CalPath::Principal` parses,
/// duplicated here (rather than exposing a public checker from that crate
/// for one call site) so this module can pass the *same* user
/// `breadcloud-caldav`'s own `handlers::discovery::principal` already
/// answers `calendar-home-set` for (that handler takes `user` from the
/// URL as-is, not from the authenticated identity -- see its own doc
/// comment -- so this splice must match that, not fall back to whatever
/// username Basic auth happens to be configured with).
fn principal_path_user(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/remote.php/dav/principals/")?;
    let user = rest.trim_end_matches('/');
    if user.is_empty() || user.contains('/') {
        None
    } else {
        Some(user.to_string())
    }
}

/// Additively splice one href-valued property, pointing at `href`, into
/// the *first* `<d:response>` block of a `multistatus` response --
/// generalizes what was originally a `current-user-principal`-only splice
/// (CLOUD-6) to also cover `addressbook-home-set` (CLOUD-7), since both
/// are "one extra href property, appended as its own `propstat`" in
/// exactly the same shape. `open_tag`/`close_tag` differ only when the
/// spliced element needs its own inline namespace declaration (CardDAV's
/// `card:` prefix is not declared by CalDAV's response root, unlike `d:`,
/// which both crates already declare) -- see the call sites above. Only
/// touches the *first* response block (the root/principal resource's own
/// entry is always listed first), so a `Depth: 1` response listing
/// children is unaffected beyond that one block, and appends a brand-new
/// `<d:propstat>` rather than editing an existing one (RFC 4918 14.22
/// allows more than one `propstat` per `response`, one per distinct
/// status). Leaves the response untouched if it is not a 207 multistatus
/// or its body cannot be read.
async fn add_href_prop(
    response: Response,
    open_tag: &str,
    close_tag: &str,
    href: &str,
) -> Response {
    if response.status() != breadcloud_webdav::error::multi_status() {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 8 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return Response::from_parts(parts, Body::empty()),
    };
    let Ok(xml) = std::str::from_utf8(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };

    let patched = splice_href_prop(xml, open_tag, close_tag, href);
    Response::from_parts(parts, Body::from(patched))
}

fn splice_href_prop(xml: &str, open_tag: &str, close_tag: &str, href: &str) -> String {
    const CLOSE_RESPONSE: &str = "  </d:response>\n";
    let Some(insert_at) = xml.find(CLOSE_RESPONSE) else {
        // Not the shape this function knows how to patch (e.g. an empty
        // or unexpected multistatus) -- return it unmodified rather than
        // risk producing invalid XML.
        return xml.to_string();
    };

    let propstat = format!(
        "    <d:propstat>\n      <d:prop>\n        <{open_tag}><d:href>{}</d:href></{close_tag}>\n      </d:prop>\n      <d:status>HTTP/1.1 200 OK</d:status>\n    </d:propstat>\n",
        xml_escape(href)
    );

    let mut out = String::with_capacity(xml.len() + propstat.len());
    out.push_str(&xml[..insert_at]);
    out.push_str(&propstat);
    out.push_str(&xml[insert_at..]);
    out
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

async fn auth_gate(State(state): State<CombinedState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();

    // These discovery responses are static and do not need an account
    // identity. Answer them here so unauthenticated requests never reach
    // dispatch, where an account would otherwise have to be selected.
    if req.method() == Method::GET && path == "/status.php" {
        return breadcloud_webdav::handlers::discovery::status();
    }
    if req.method() == Method::GET && path == "/.well-known/caldav" {
        return breadcloud_caldav::handlers::discovery::well_known();
    }
    if req.method() == Method::GET && path == "/.well-known/carddav" {
        return breadcloud_carddav::handlers::discovery::well_known();
    }

    if state.accounts.is_empty() {
        // Open mode: no accounts configured at all, server runs
        // unauthenticated (unchanged from before CLOUD-11). Every request
        // operates as the single implicit default account.
        req.extensions_mut()
            .insert(AuthenticatedUser(state.default_user.clone()));
        return next.run(req).await;
    }

    match state.accounts.authenticate_request(req.headers()) {
        Some(username) => {
            req.extensions_mut().insert(AuthenticatedUser(username));
            next.run(req).await
        }
        None => unauthorized(),
    }
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response.headers_mut().insert(
        "WWW-Authenticate",
        "Basic realm=\"breadcloud\"".parse().unwrap(),
    );
    response
}

/// CLOUD-11: a request whose URL names a different account than the one
/// that actually authenticated (see `dispatch`'s `url_scoped_user`
/// check).
fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "the authenticated account does not own this path",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use breadcloud_storage::{AddressbookStore, CalendarStore, Storage, TrashStore, VersionStore};
    use breadcloud_webdav::error::multi_status;
    use breadcloud_webdav::Account;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    /// 1 GiB, matching `main.rs`'s own constant of the same name (not
    /// shared across the `main`/`combined` module boundary since only
    /// tests here need it).
    const GIB: u64 = 1024 * 1024 * 1024;

    /// Every real storage root the combined server needs, kept alive for
    /// the lifetime of a test so their `TempDir`s aren't dropped (and
    /// deleted) out from under the router mid-test.
    struct TestDirs {
        _storage: TempDir,
        _uploads: TempDir,
        _versions: TempDir,
        _trash: TempDir,
        _calendars: TempDir,
        _addressbooks: TempDir,
    }

    /// Single-account, open-mode (no `Authorization` required) test app --
    /// what every pre-CLOUD-11 test in this module already assumed. Kept
    /// exactly as it was so none of those tests had to change.
    async fn test_app() -> (TestDirs, Router) {
        let storage_dir = TempDir::new().unwrap();
        let uploads_dir = TempDir::new().unwrap();
        let versions_dir = TempDir::new().unwrap();
        let trash_dir = TempDir::new().unwrap();
        let calendars_dir = TempDir::new().unwrap();
        let addressbooks_dir = TempDir::new().unwrap();

        let storage = Storage::new(storage_dir.path());
        let uploads = Storage::new(uploads_dir.path());
        let versions = VersionStore::new(versions_dir.path());
        let trash = TrashStore::new(trash_dir.path());
        let webdav =
            breadcloud_webdav::AppState::new(storage, uploads, versions, trash).with_owner("dev");

        let calendars = CalendarStore::new(calendars_dir.path());
        calendars
            .ensure_calendar("dev", "personal", "Personal", None)
            .await
            .unwrap();
        let caldav = breadcloud_caldav::AppState::new(calendars);

        let addressbooks = AddressbookStore::new(addressbooks_dir.path());
        addressbooks
            .ensure_addressbook("dev", "contacts", "Contacts")
            .await
            .unwrap();
        let carddav = breadcloud_carddav::AppState::new(addressbooks);

        let dirs = TestDirs {
            _storage: storage_dir,
            _uploads: uploads_dir,
            _versions: versions_dir,
            _trash: trash_dir,
            _calendars: calendars_dir,
            _addressbooks: addressbooks_dir,
        };
        let webdav_by_user = HashMap::from([("dev".to_string(), webdav)]);
        (
            dirs,
            router(
                AccountStore::default(),
                webdav_by_user,
                caldav,
                carddav,
                "dev",
            ),
        )
    }

    async fn propfind(app: &Router, uri: &str, depth: &str) -> Response {
        let req = Request::builder()
            .method("PROPFIND")
            .uri(uri)
            .header("Depth", depth)
            .body(Body::empty())
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    async fn body_string(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn basic_auth(username: &str, password: &str) -> String {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        format!("Basic {encoded}")
    }

    /// One request as `username`, with a real password checked by the
    /// combined server's account store. `body` is sent as-is (empty for
    /// GET/DELETE/PROPFIND-with-no-body callers).
    async fn request_as(
        app: &Router,
        method: &str,
        uri: &str,
        username: &str,
        password: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", basic_auth(username, password));
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::from(body)).unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    /// A combined app with `users` as real, independently-authenticated,
    /// independently-quota'd accounts (CLOUD-11) -- each entry is
    /// `(username, password, quota_bytes)`. Every account gets its own
    /// genuinely separate `Storage`/`VersionStore`/`TrashStore` (a
    /// subdirectory of the shared `TestDirs` roots, exactly like the real
    /// server's `main.rs` does against `BREADCLOUD_ROOT`), and its own
    /// `personal` calendar and `contacts` addressbook (sharing one
    /// `CalendarStore`/`AddressbookStore` root, same as production --
    /// those already namespace by user internally).
    async fn multi_user_test_app(users: &[(&str, &str, u64)]) -> (TestDirs, Router) {
        let storage_dir = TempDir::new().unwrap();
        let uploads_dir = TempDir::new().unwrap();
        let versions_dir = TempDir::new().unwrap();
        let trash_dir = TempDir::new().unwrap();
        let calendars_dir = TempDir::new().unwrap();
        let addressbooks_dir = TempDir::new().unwrap();

        let calendars = CalendarStore::new(calendars_dir.path());
        let addressbooks = AddressbookStore::new(addressbooks_dir.path());

        let mut accounts = Vec::new();
        let mut webdav_by_user = HashMap::new();
        for (username, password, quota_bytes) in users {
            let user_storage = storage_dir.path().join(username);
            let user_uploads = uploads_dir.path().join(username);
            let user_versions = versions_dir.path().join(username);
            let user_trash = trash_dir.path().join(username);
            for dir in [&user_storage, &user_uploads, &user_versions, &user_trash] {
                std::fs::create_dir_all(dir).unwrap();
            }

            let state = breadcloud_webdav::AppState::new(
                Storage::new(user_storage),
                Storage::new(user_uploads),
                VersionStore::new(user_versions),
                TrashStore::new(user_trash),
            )
            .with_owner(*username)
            .with_quota_limit(*quota_bytes);
            webdav_by_user.insert(username.to_string(), state);

            calendars
                .ensure_calendar(username, "personal", "Personal", None)
                .await
                .unwrap();
            addressbooks
                .ensure_addressbook(username, "contacts", "Contacts")
                .await
                .unwrap();

            accounts.push(Account::new(*username, password, *quota_bytes));
        }

        let caldav = breadcloud_caldav::AppState::new(calendars);
        let carddav = breadcloud_carddav::AppState::new(addressbooks);
        let default_user = users[0].0.to_string();

        let dirs = TestDirs {
            _storage: storage_dir,
            _uploads: uploads_dir,
            _versions: versions_dir,
            _trash: trash_dir,
            _calendars: calendars_dir,
            _addressbooks: addressbooks_dir,
        };
        (
            dirs,
            router(
                AccountStore::new(accounts),
                webdav_by_user,
                caldav,
                carddav,
                default_user,
            ),
        )
    }

    // This is the exact regression this fix targets (CLOUD-6 change
    // request, reproduced live against vdirsyncer pointed at the bare
    // server root): before this fix, a PROPFIND at "/" fell through to
    // `breadcloud_webdav::dispatch` and came back a valid-but-incomplete
    // 207 with no `current-user-principal`, which is a *success* by the
    // shape a real client's discovery loop checks for retrying
    // `.well-known/caldav` -- so discovery dead-ended right here instead
    // of ever reaching a calendar. It does not reproduce vdirsyncer's own
    // retry logic (that lives in vdirsyncer, not this server), but it
    // does assert the one thing this server controls: that the property a
    // "just enter a server address" client is looking for is actually in
    // the bare-root response now.
    #[tokio::test]
    async fn bare_root_propfind_includes_current_user_principal() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/", "0").await;
        assert_eq!(response.status(), StatusCode::from_u16(207).unwrap());

        let xml = body_string(response).await;
        // CLOUD-11: open mode's implicit account is consistently "dev"
        // everywhere now (matching what `test_app` already bootstraps its
        // calendar/addressbook under) -- before CLOUD-11 this reported
        // "anonymous" here specifically, a latent inconsistency
        // (`principal_href` fell back to a different placeholder than
        // every other open-mode code path already used) that resolving
        // the account once, centrally, and reusing it everywhere fixes as
        // a side effect.
        assert!(
            xml.contains(
                "<d:current-user-principal><d:href>/remote.php/dav/principals/dev/</d:href></d:current-user-principal>"
            ),
            "expected current-user-principal in bare-root PROPFIND response, got: {xml}"
        );
    }

    // The injection targets only the root resource's own response block,
    // so a Depth: 1 listing (which lists children too, none here) still
    // carries exactly one current-user-principal, not one per entry.
    #[tokio::test]
    async fn current_user_principal_appears_exactly_once_at_depth_one() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/", "1").await;
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<d:current-user-principal>").count(), 1);
    }

    // A PROPFIND on some other WebDAV path is untouched: no
    // current-user-principal gets spliced into anything but the bare
    // root, and the ordinary resourcetype/getetag WebDAV properties this
    // crate already tests (CLOUD-2/3) still come through unmodified.
    #[tokio::test]
    async fn non_root_webdav_propfind_is_unaffected() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/does-not-exist", "0").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // The bare-root CalDAV discovery path (`/remote.php/dav/`, which
    // `owns_path` *does* claim) already answers with its own
    // `current-user-principal`, unaffected by this change: `dispatch`
    // returns straight out of the `owns_path` branch before the
    // bare-root splice logic ever runs (it goes through the principal
    // splice branch instead, but this is Depth 0 on the DAV root itself,
    // not the principal resource, so no addressbook-home-set splice
    // applies here either).
    #[tokio::test]
    async fn caldav_root_path_is_dispatched_to_caldav_unmodified() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/remote.php/dav/", "0").await;
        assert_eq!(response.status(), StatusCode::from_u16(207).unwrap());
        let xml = body_string(response).await;
        assert_eq!(xml.matches("<d:current-user-principal>").count(), 1);
    }

    // CLOUD-7: the principal PROPFIND response (the second hop of
    // discovery, already carrying `cal:calendar-home-set`) now also
    // carries `card:addressbook-home-set`, spliced on by this module --
    // "discover the addressbook the same way DAVx5/iOS already discover
    // a calendar".
    #[tokio::test]
    async fn principal_propfind_includes_addressbook_home_set() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/remote.php/dav/principals/dev/", "0").await;
        assert_eq!(response.status(), StatusCode::from_u16(207).unwrap());
        let xml = body_string(response).await;
        assert!(
            xml.contains("<card:addressbook-home-set"),
            "expected addressbook-home-set in principal PROPFIND response, got: {xml}"
        );
        assert!(
            xml.contains("/remote.php/dav/addressbooks/dev/</d:href></card:addressbook-home-set>")
        );
        // calendar-home-set (breadcloud-caldav's own, untouched) is still
        // there too -- this is additive, not a replacement.
        assert!(xml.contains("<cal:calendar-home-set>"));
    }

    // A CalDAV path that is not the principal resource (a calendar
    // collection PROPFIND) is dispatched unmodified: no
    // addressbook-home-set splice applies outside the principal hop.
    #[tokio::test]
    async fn non_principal_caldav_propfind_is_unaffected() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/remote.php/dav/calendars/dev/personal/", "0").await;
        let xml = body_string(response).await;
        assert!(!xml.contains("addressbook-home-set"));
    }

    // CardDAV requests are routed to breadcloud_carddav::dispatch and
    // answer normally under the combined app's single shared auth gate.
    #[tokio::test]
    async fn carddav_addressbook_home_propfind_is_dispatched() {
        let (_dirs, app) = test_app().await;
        let response = propfind(&app, "/remote.php/dav/addressbooks/dev/", "1").await;
        assert_eq!(response.status(), StatusCode::from_u16(207).unwrap());
        let xml = body_string(response).await;
        assert!(xml.contains("contacts/</d:href>"));
    }

    #[tokio::test]
    async fn well_known_carddav_is_exempt_from_auth() {
        // Real accounts configured this time (unlike `test_app`'s open
        // mode), to confirm the `.well-known/carddav` exemption in
        // `auth_gate` fires *before* the accounts check, not just when
        // there happen to be no accounts to check against.
        let (_dirs, app) = multi_user_test_app(&[("dev", "secret", 100 * GIB)]).await;

        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/carddav")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    }

    // --- CLOUD-11: multi-user auth, per-user isolation, and quota -----

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;
        let response =
            request_as(&app, "GET", "/note.txt", "alice", "wrong", &[], Vec::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_credentials_are_rejected_when_accounts_are_configured() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;
        let req = Request::builder()
            .method("GET")
            .uri("/note.txt")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn status_php_paths_do_not_bypass_auth_for_web_ui_files() {
        let (dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let file = dirs._storage.path().join("alice/site/status.php");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "private site content").unwrap();

        let folder = dirs._storage.path().join("alice/status.php");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("secret.txt"), "private folder content").unwrap();

        for uri in ["/ui/download/site/status.php", "/ui/files/status.php"] {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        let status = Request::builder()
            .method("GET")
            .uri("/status.php")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(status).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_string(response).await.contains("\"installed\":true"));
    }

    /// The core file-isolation regression: what user A writes must be
    /// completely invisible to user B, not just "differently labeled" --
    /// each account gets a genuinely separate `Storage` root, not a
    /// shared one namespaced by a URL segment.
    #[tokio::test]
    async fn files_are_isolated_between_accounts() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let put = request_as(
            &app,
            "PUT",
            "/note.txt",
            "alice",
            "secret1",
            &[],
            b"alice's private note".to_vec(),
        )
        .await;
        assert_eq!(put.status(), StatusCode::CREATED);

        // Bob, authenticated as himself, requesting the exact same path:
        // his own tree has no such file.
        let bob_get = request_as(&app, "GET", "/note.txt", "bob", "secret2", &[], Vec::new()).await;
        assert_eq!(bob_get.status(), StatusCode::NOT_FOUND);

        // Alice still sees her own content, untouched.
        let alice_get = request_as(
            &app,
            "GET",
            "/note.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(alice_get.status(), StatusCode::OK);
        assert_eq!(body_string(alice_get).await, "alice's private note");
    }

    /// A client authenticated as one account cannot read or write another
    /// account's tree by naming it in the Nextcloud-style
    /// `/remote.php/dav/files/<user>/...` URL, even with otherwise-valid
    /// credentials for a *different* account: before CLOUD-11 this
    /// segment was accepted as a label, not checked against who actually
    /// authenticated.
    #[tokio::test]
    async fn url_account_segment_must_match_the_authenticated_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        request_as(
            &app,
            "PUT",
            "/remote.php/dav/files/alice/secret.txt",
            "alice",
            "secret1",
            &[],
            b"alice's data".to_vec(),
        )
        .await;

        // Bob authenticates correctly as himself, but the URL names
        // alice's account.
        let response = request_as(
            &app,
            "GET",
            "/remote.php/dav/files/alice/secret.txt",
            "bob",
            "secret2",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The same URL-vs-authenticated-account check, but for the versions
    /// and trashbin namespaces (CLOUD-4/CLOUD-5's surfaces) rather than
    /// the main files tree.
    #[tokio::test]
    async fn versions_and_trash_url_segments_are_also_checked() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let versions_response = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/versions/alice/note.txt",
            "bob",
            "secret2",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(versions_response.status(), StatusCode::FORBIDDEN);

        let trash_response = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/trashbin/alice/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(trash_response.status(), StatusCode::FORBIDDEN);
    }

    /// Real (not just URL-blocked) isolation for version history and
    /// trash: bob's own versions/trash surfaces, under his own account,
    /// genuinely do not contain anything of alice's.
    #[tokio::test]
    async fn versions_and_trash_are_genuinely_separate_per_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        // Alice creates a file, overwrites it (a version), then deletes
        // it (moves it to trash).
        request_as(
            &app,
            "PUT",
            "/shared-name.txt",
            "alice",
            "secret1",
            &[],
            b"version one".to_vec(),
        )
        .await;
        request_as(
            &app,
            "PUT",
            "/shared-name.txt",
            "alice",
            "secret1",
            &[],
            b"version two".to_vec(),
        )
        .await;
        request_as(
            &app,
            "DELETE",
            "/shared-name.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;

        // Alice's own versions/trash surfaces do show her data.
        let alice_versions = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/versions/alice/shared-name.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(alice_versions.status(), multi_status());

        let alice_trash = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/trashbin/alice/",
            "alice",
            "secret1",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        let alice_trash_xml = body_string(alice_trash).await;
        assert!(alice_trash_xml.contains("shared-name.txt"));

        // Bob, browsing his *own* versions surface under a file path of
        // the exact same name, finds nothing: a genuinely separate
        // VersionStore, not a shared one that just happened to be gated
        // by the URL check above. (A version listing for a path with no
        // history -- including one that never existed at all -- is a
        // valid empty 207, not a 404; the isolation signal here is that
        // it is empty, not that it errors.)
        let bob_versions = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/versions/bob/shared-name.txt",
            "bob",
            "secret2",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(bob_versions.status(), multi_status());
        let bob_versions_xml = body_string(bob_versions).await;
        assert!(!bob_versions_xml.contains("<d:response>"));

        let bob_trash = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/trashbin/bob/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        let bob_trash_xml = body_string(bob_trash).await;
        assert!(!bob_trash_xml.contains("shared-name.txt"));
    }

    /// Calendar isolation (CLOUD-6): an event alice creates in her own
    /// `personal` calendar never appears when bob lists his own
    /// same-named calendar.
    #[tokio::test]
    async fn calendars_are_isolated_between_accounts() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let ics = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:alice-event-1\r\nDTSTART:20260401T090000Z\r\nDTEND:20260401T100000Z\r\nSUMMARY:Alice only\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_vec();
        let put = request_as(
            &app,
            "PUT",
            "/remote.php/dav/calendars/alice/personal/alice-event-1.ics",
            "alice",
            "secret1",
            &[],
            ics,
        )
        .await;
        assert_eq!(put.status(), StatusCode::CREATED);

        // Bob cannot reach alice's calendar at all (URL check).
        let cross = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/calendars/alice/personal/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(cross.status(), StatusCode::FORBIDDEN);

        // And bob's own, same-named calendar genuinely has no such event.
        let bobs_own = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/calendars/bob/personal/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(bobs_own.status(), multi_status());
        let xml = body_string(bobs_own).await;
        assert!(!xml.contains("alice-event-1"));
    }

    /// Addressbook isolation (CLOUD-7): the same shape as the calendar
    /// test above, for a contact instead of an event.
    #[tokio::test]
    async fn addressbooks_are_isolated_between_accounts() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let vcard = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:alice-contact-1\r\nFN:Alice's Friend\r\nEND:VCARD\r\n".to_vec();
        let put = request_as(
            &app,
            "PUT",
            "/remote.php/dav/addressbooks/alice/contacts/alice-contact-1.vcf",
            "alice",
            "secret1",
            &[],
            vcard,
        )
        .await;
        assert_eq!(put.status(), StatusCode::CREATED);

        let cross = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/addressbooks/alice/contacts/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(cross.status(), StatusCode::FORBIDDEN);

        let bobs_own = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/addressbooks/bob/contacts/",
            "bob",
            "secret2",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(bobs_own.status(), multi_status());
        let xml = body_string(bobs_own).await;
        assert!(!xml.contains("alice-contact-1"));
    }

    #[tokio::test]
    async fn caldav_and_carddav_writes_and_deletes_honor_preconditions() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let event_path = "/remote.php/dav/calendars/alice/personal/conditional.ics";
        let event = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:conditional-event\r\nDTSTART:20260401T090000Z\r\nDTEND:20260401T100000Z\r\nSUMMARY:Initial event\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_vec();
        let event_create = request_as(
            &app,
            "PUT",
            event_path,
            "alice",
            "secret1",
            &[("If-None-Match", "*")],
            event.clone(),
        )
        .await;
        assert_eq!(event_create.status(), StatusCode::CREATED);

        let event_duplicate_create = request_as(
            &app,
            "PUT",
            event_path,
            "alice",
            "secret1",
            &[("If-None-Match", "*")],
            event.clone(),
        )
        .await;
        assert_eq!(
            event_duplicate_create.status(),
            StatusCode::PRECONDITION_FAILED
        );

        let event_read =
            request_as(&app, "GET", event_path, "alice", "secret1", &[], Vec::new()).await;
        let event_etag = event_read
            .headers()
            .get("ETag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let event_stale_update = request_as(
            &app,
            "PUT",
            event_path,
            "alice",
            "secret1",
            &[("If-Match", "\"stale\"")],
            event.clone(),
        )
        .await;
        assert_eq!(event_stale_update.status(), StatusCode::PRECONDITION_FAILED);
        let event_stale_delete = request_as(
            &app,
            "DELETE",
            event_path,
            "alice",
            "secret1",
            &[("If-Match", "\"stale\"")],
            Vec::new(),
        )
        .await;
        assert_eq!(event_stale_delete.status(), StatusCode::PRECONDITION_FAILED);
        let event_delete = request_as(
            &app,
            "DELETE",
            event_path,
            "alice",
            "secret1",
            &[("If-Match", &event_etag)],
            Vec::new(),
        )
        .await;
        assert_eq!(event_delete.status(), StatusCode::NO_CONTENT);

        let contact_path = "/remote.php/dav/addressbooks/alice/contacts/conditional.vcf";
        let contact = b"BEGIN:VCARD\r\nVERSION:4.0\r\nUID:conditional-contact\r\nFN:Initial Contact\r\nEND:VCARD\r\n".to_vec();
        let contact_create = request_as(
            &app,
            "PUT",
            contact_path,
            "alice",
            "secret1",
            &[("If-None-Match", "*")],
            contact.clone(),
        )
        .await;
        assert_eq!(contact_create.status(), StatusCode::CREATED);

        let contact_duplicate_create = request_as(
            &app,
            "PUT",
            contact_path,
            "alice",
            "secret1",
            &[("If-None-Match", "*")],
            contact.clone(),
        )
        .await;
        assert_eq!(
            contact_duplicate_create.status(),
            StatusCode::PRECONDITION_FAILED
        );

        let contact_read = request_as(
            &app,
            "GET",
            contact_path,
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        let contact_etag = contact_read
            .headers()
            .get("ETag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let contact_stale_update = request_as(
            &app,
            "PUT",
            contact_path,
            "alice",
            "secret1",
            &[("If-Match", "\"stale\"")],
            contact,
        )
        .await;
        assert_eq!(
            contact_stale_update.status(),
            StatusCode::PRECONDITION_FAILED
        );
        let contact_stale_delete = request_as(
            &app,
            "DELETE",
            contact_path,
            "alice",
            "secret1",
            &[("If-Match", "\"stale\"")],
            Vec::new(),
        )
        .await;
        assert_eq!(
            contact_stale_delete.status(),
            StatusCode::PRECONDITION_FAILED
        );
        let contact_delete = request_as(
            &app,
            "DELETE",
            contact_path,
            "alice",
            "secret1",
            &[("If-Match", &contact_etag)],
            Vec::new(),
        )
        .await;
        assert_eq!(contact_delete.status(), StatusCode::NO_CONTENT);
    }

    /// The bare-root `current-user-principal` splice (CLOUD-6) must
    /// advertise whichever account actually authenticated this specific
    /// request, not a single fixed account -- the bug `principal_href_for`
    /// exists to close (see `dispatch`'s doc comment).
    #[tokio::test]
    async fn bare_root_principal_reflects_the_authenticated_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let alice_response = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let alice_xml = body_string(alice_response).await;
        assert!(alice_xml.contains("/remote.php/dav/principals/alice/"));

        let bob_response = request_as(
            &app,
            "PROPFIND",
            "/",
            "bob",
            "secret2",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let bob_xml = body_string(bob_response).await;
        assert!(bob_xml.contains("/remote.php/dav/principals/bob/"));
    }

    #[tokio::test]
    async fn put_within_quota_succeeds() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100)]).await;
        let response = request_as(
            &app,
            "PUT",
            "/small.txt",
            "alice",
            "secret1",
            &[],
            b"fifty bytes".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn put_exceeding_quota_is_rejected_with_507() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 10)]).await;
        let response = request_as(
            &app,
            "PUT",
            "/too-big.txt",
            "alice",
            "secret1",
            &[],
            b"this is way more than ten bytes".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::from_u16(507).unwrap());

        // The rejected write must never have landed.
        let get = request_as(
            &app,
            "GET",
            "/too-big.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(get.status(), StatusCode::NOT_FOUND);
    }

    /// A quota-rejected overwrite must leave the existing content exactly
    /// as it was -- not truncated, not partially replaced.
    #[tokio::test]
    async fn quota_rejected_overwrite_does_not_corrupt_existing_content() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 20)]).await;

        let first = request_as(
            &app,
            "PUT",
            "/f.txt",
            "alice",
            "secret1",
            &[],
            b"0123456789".to_vec(), // 10 bytes, well within the 20-byte quota
        )
        .await;
        assert_eq!(first.status(), StatusCode::CREATED);

        // A larger overwrite that alone would exceed the quota once the
        // existing 10 bytes are accounted for.
        let second = request_as(
            &app,
            "PUT",
            "/f.txt",
            "alice",
            "secret1",
            &[],
            b"this replacement content is much longer than twenty bytes".to_vec(),
        )
        .await;
        assert_eq!(second.status(), StatusCode::from_u16(507).unwrap());

        let get = request_as(&app, "GET", "/f.txt", "alice", "secret1", &[], Vec::new()).await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(body_string(get).await, "0123456789");

        let versions = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/versions/alice/f.txt",
            "alice",
            "secret1",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(versions.status(), multi_status());
        assert!(!body_string(versions).await.contains("<d:response>"));
    }

    #[tokio::test]
    async fn quota_rejected_ui_overwrite_does_not_create_a_version() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 20)]).await;
        let first = upload_via_ui(&app, "alice", "secret1", "/", "f.txt", b"0123456789").await;
        assert_eq!(first.status(), StatusCode::SEE_OTHER);

        let rejected = upload_via_ui(
            &app,
            "alice",
            "secret1",
            "/",
            "f.txt",
            b"replacement exceeds the remaining quota",
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::from_u16(507).unwrap());

        let versions = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/versions/alice/f.txt",
            "alice",
            "secret1",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert_eq!(versions.status(), multi_status());
        assert!(!body_string(versions).await.contains("<d:response>"));
    }

    /// Quota is per-user, not shared: alice being nearly out of quota
    /// must not affect bob, who has his own separate allotment.
    #[tokio::test]
    async fn quota_is_independent_per_account() {
        let (_dirs, app) =
            multi_user_test_app(&[("alice", "secret1", 10), ("bob", "secret2", 10 * GIB)]).await;

        let alice_put = request_as(
            &app,
            "PUT",
            "/f.txt",
            "alice",
            "secret1",
            &[],
            b"way over alice's ten-byte quota".to_vec(),
        )
        .await;
        assert_eq!(alice_put.status(), StatusCode::from_u16(507).unwrap());

        // Bob's much larger quota is untouched by alice's rejected write.
        let bob_put = request_as(
            &app,
            "PUT",
            "/f.txt",
            "bob",
            "secret2",
            &[],
            b"well within bob's ten-gib quota".to_vec(),
        )
        .await;
        assert_eq!(bob_put.status(), StatusCode::CREATED);
    }

    /// PROPFIND's `quota-used-bytes` must reflect each account's own
    /// usage, not a shared or filesystem-wide figure.
    #[tokio::test]
    async fn propfind_quota_used_bytes_is_per_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        request_as(
            &app,
            "PUT",
            "/ten-bytes.txt",
            "alice",
            "secret1",
            &[],
            b"0123456789".to_vec(),
        )
        .await;

        let alice_root = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let alice_xml = body_string(alice_root).await;
        assert!(
            alice_xml.contains("<d:quota-used-bytes>10</d:quota-used-bytes>"),
            "expected alice's usage to be 10 bytes, got: {alice_xml}"
        );

        // Bob wrote nothing: his own root reports zero usage, not
        // alice's 10 bytes and not a shared/filesystem total.
        let bob_root = request_as(
            &app,
            "PROPFIND",
            "/",
            "bob",
            "secret2",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let bob_xml = body_string(bob_root).await;
        assert!(
            bob_xml.contains("<d:quota-used-bytes>0</d:quota-used-bytes>"),
            "expected bob's usage to be 0 bytes, got: {bob_xml}"
        );
    }

    /// Pull the numeric value out of a PROPFIND response's
    /// `<d:quota-used-bytes>` element, for a test that needs to assert on
    /// the actual figure rather than just a fixed substring.
    fn extract_quota_used_bytes(xml: &str) -> u64 {
        let tag = "<d:quota-used-bytes>";
        let start = xml
            .find(tag)
            .unwrap_or_else(|| panic!("response has no {tag}: {xml}"))
            + tag.len();
        let end = xml[start..]
            .find('<')
            .unwrap_or_else(|| panic!("unterminated {tag} in: {xml}"))
            + start;
        xml[start..end]
            .parse()
            .unwrap_or_else(|_| panic!("quota-used-bytes was not numeric: {xml}"))
    }

    /// CLOUD-11 rework: the TOCTOU the reviewer reproduced live, closed.
    /// Two concurrent PUTs against the same account, each individually
    /// well under quota, that together exceed it -- matching the live
    /// repro in shape (two real ~700MB PUTs against a 1 GiB quota, both
    /// landing at ~1.47GB, ~37% over), scaled down to run fast and
    /// deterministically here. Dispatched on a real multi-threaded
    /// runtime via `tokio::spawn` so the two requests genuinely overlap,
    /// not just race the test executor's own scheduling order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_near_quota_boundary_never_together_exceed_it() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 1000)]).await;

        let app_a = app.clone();
        let app_b = app.clone();
        let task_a = tokio::spawn(async move {
            request_as(
                &app_a,
                "PUT",
                "/a.bin",
                "alice",
                "secret1",
                &[],
                vec![b'a'; 700],
            )
            .await
            .status()
        });
        let task_b = tokio::spawn(async move {
            request_as(
                &app_b,
                "PUT",
                "/b.bin",
                "alice",
                "secret1",
                &[],
                vec![b'b'; 700],
            )
            .await
            .status()
        });

        let status_a = task_a.await.unwrap();
        let status_b = task_b.await.unwrap();

        let insufficient_storage = StatusCode::from_u16(507).unwrap();
        let outcomes = [status_a, status_b];
        let created = outcomes
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let rejected = outcomes
            .iter()
            .filter(|s| **s == insufficient_storage)
            .count();

        // Each write (700 bytes) is individually well under the
        // 1000-byte quota; only their sum (1400) exceeds it. Before the
        // CLOUD-11 quota-gate fix, both of these landed -- this is the
        // regression test for that exact race.
        assert_eq!(
            (created, rejected),
            (1, 1),
            "exactly one of the two concurrent, individually-in-quota writes must be \
             accepted and the other rejected with 507, got {status_a:?}/{status_b:?}"
        );

        let propfind = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let xml = body_string(propfind).await;
        let used = extract_quota_used_bytes(&xml);
        assert!(
            used <= 1000,
            "final reported usage {used} must never exceed the 1000-byte quota, xml: {xml}"
        );
    }

    /// CLOUD-11 round 3 fix, re-verified against the exact mix the
    /// round-2 reviewer specifically asked for: a plain PUT racing a
    /// chunked-upload finalize on the same account, each individually
    /// well under quota, together exceeding it. Before the round-3 fix
    /// this pairing could produce two rejections rather than one accept
    /// and one reject: the finalize path double-counted its own staged
    /// assembly against quota, so it could 507 even without anything
    /// racing it. This confirms the fix still serializes the mixed case
    /// the same way plain-PUT-vs-plain-PUT already does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_plain_put_and_chunked_finalize_near_quota_boundary_never_together_exceed_it(
    ) {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 1000)]).await;

        // Stage the chunk ahead of time so the race is on the quota-gated
        // finalize step itself, not on ordinary chunk upload (which is
        // not quota-gated).
        let mkcol = request_as(
            &app,
            "MKCOL",
            "/remote.php/dav/uploads/alice/txn1",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(mkcol.status(), StatusCode::CREATED);

        let chunk_body = vec![b'c'; 700];
        let put_chunk = request_as(
            &app,
            "PUT",
            "/remote.php/dav/uploads/alice/txn1/1",
            "alice",
            "secret1",
            &[],
            chunk_body.clone(),
        )
        .await;
        assert_eq!(put_chunk.status(), StatusCode::CREATED);

        let app_a = app.clone();
        let task_a = tokio::spawn(async move {
            request_as(
                &app_a,
                "PUT",
                "/a.bin",
                "alice",
                "secret1",
                &[],
                vec![b'a'; 700],
            )
            .await
            .status()
        });

        let app_b = app.clone();
        let total_len = chunk_body.len().to_string();
        let task_b = tokio::spawn(async move {
            request_as(
                &app_b,
                "MOVE",
                "/remote.php/dav/uploads/alice/txn1/.file",
                "alice",
                "secret1",
                &[("Destination", "/b.bin"), ("OC-Total-Length", &total_len)],
                Vec::new(),
            )
            .await
            .status()
        });

        let status_a = task_a.await.unwrap();
        let status_b = task_b.await.unwrap();

        let insufficient_storage = StatusCode::from_u16(507).unwrap();
        let outcomes = [status_a, status_b];
        let created = outcomes
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let rejected = outcomes
            .iter()
            .filter(|s| **s == insufficient_storage)
            .count();

        assert_eq!(
            (created, rejected),
            (1, 1),
            "exactly one of a concurrent plain PUT and chunked finalize, each individually \
             within quota, must be accepted and the other rejected with 507, got \
             {status_a:?}/{status_b:?}"
        );

        let propfind = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let xml = body_string(propfind).await;
        let used = extract_quota_used_bytes(&xml);
        assert!(
            used <= 1000,
            "final reported usage {used} must never exceed the 1000-byte quota, xml: {xml}"
        );
    }

    /// Same mix, but both sides are chunked-upload finalizes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_chunked_finalizes_near_quota_boundary_never_together_exceed_it() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 1000)]).await;

        for txn in ["txn-a", "txn-b"] {
            let mkcol = request_as(
                &app,
                "MKCOL",
                &format!("/remote.php/dav/uploads/alice/{txn}"),
                "alice",
                "secret1",
                &[],
                Vec::new(),
            )
            .await;
            assert_eq!(mkcol.status(), StatusCode::CREATED);

            let put_chunk = request_as(
                &app,
                "PUT",
                &format!("/remote.php/dav/uploads/alice/{txn}/1"),
                "alice",
                "secret1",
                &[],
                vec![b'x'; 700],
            )
            .await;
            assert_eq!(put_chunk.status(), StatusCode::CREATED);
        }

        let app_a = app.clone();
        let task_a = tokio::spawn(async move {
            request_as(
                &app_a,
                "MOVE",
                "/remote.php/dav/uploads/alice/txn-a/.file",
                "alice",
                "secret1",
                &[("Destination", "/a.bin"), ("OC-Total-Length", "700")],
                Vec::new(),
            )
            .await
            .status()
        });
        let app_b = app.clone();
        let task_b = tokio::spawn(async move {
            request_as(
                &app_b,
                "MOVE",
                "/remote.php/dav/uploads/alice/txn-b/.file",
                "alice",
                "secret1",
                &[("Destination", "/b.bin"), ("OC-Total-Length", "700")],
                Vec::new(),
            )
            .await
            .status()
        });

        let status_a = task_a.await.unwrap();
        let status_b = task_b.await.unwrap();

        let insufficient_storage = StatusCode::from_u16(507).unwrap();
        let outcomes = [status_a, status_b];
        let created = outcomes
            .iter()
            .filter(|s| **s == StatusCode::CREATED)
            .count();
        let rejected = outcomes
            .iter()
            .filter(|s| **s == insufficient_storage)
            .count();

        assert_eq!(
            (created, rejected),
            (1, 1),
            "exactly one of two concurrent chunked finalizes, each individually within \
             quota, must be accepted and the other rejected with 507, got \
             {status_a:?}/{status_b:?}"
        );

        let propfind = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0")],
            Vec::new(),
        )
        .await;
        let xml = body_string(propfind).await;
        let used = extract_quota_used_bytes(&xml);
        assert!(
            used <= 1000,
            "final reported usage {used} must never exceed the 1000-byte quota, xml: {xml}"
        );
    }

    // --- CLOUD-12: the minimal web UI ----------------------------------
    //
    // These exercise `breadcloud_webui::dispatch` through this module's own
    // `router()`, the same combined app a real request hits -- not the
    // `breadcloud-webui` crate's handlers in isolation -- so they cover
    // exactly the thing CLOUD-12 asks to be verified against: the real
    // multi-account auth gate, real per-user `Storage`, and the real
    // shared `CalendarStore`, wired together the way `main.rs` wires them.

    fn multipart_body(
        boundary: &str,
        form_fields: &[(&str, &str)],
        file: (&str, &str, &[u8]),
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (name, value) in form_fields {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                    .as_bytes(),
            );
        }
        let (field_name, filename, content) = file;
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field_name}\"; \
                 filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    async fn upload_via_ui(
        app: &Router,
        user: &str,
        pass: &str,
        dir: &str,
        filename: &str,
        content: &[u8],
    ) -> Response {
        let boundary = "----breadcloudUiTestBoundary";
        let body = multipart_body(boundary, &[("dir", dir)], ("file", filename, content));
        let content_type = format!("multipart/form-data; boundary={boundary}");
        request_as(
            app,
            "POST",
            "/ui/upload",
            user,
            pass,
            &[("Content-Type", &content_type)],
            body,
        )
        .await
    }

    #[tokio::test]
    async fn ui_home_page_requires_auth_and_links_to_files_and_calendar() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let unauthed = Request::builder()
            .method("GET")
            .uri("/ui")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(unauthed).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = request_as(&app, "GET", "/ui/", "alice", "secret1", &[], Vec::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body_string(response).await;
        assert!(html.contains("/ui/files/"));
        assert!(html.contains("/ui/calendar"));
    }

    #[tokio::test]
    async fn ui_upload_lands_on_real_storage_and_is_fetchable_via_webdav_and_download() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let upload = upload_via_ui(
            &app,
            "alice",
            "secret1",
            "/",
            "hello.txt",
            b"hello from the web ui",
        )
        .await;
        assert_eq!(upload.status(), StatusCode::SEE_OTHER);

        // The very same content is reachable through plain WebDAV GET --
        // the upload went through `Storage::write_bytes` on the account's
        // real root, not a separate UI-only store.
        let webdav_get = request_as(
            &app,
            "GET",
            "/hello.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(webdav_get.status(), StatusCode::OK);
        assert_eq!(body_string(webdav_get).await, "hello from the web ui");

        let download = request_as(
            &app,
            "GET",
            "/ui/download/hello.txt",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(download.status(), StatusCode::OK);
        assert_eq!(body_string(download).await, "hello from the web ui");
    }

    #[tokio::test]
    async fn ui_files_listing_shows_uploads_and_stays_isolated_per_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        upload_via_ui(&app, "alice", "secret1", "/", "alice-note.txt", b"private").await;

        let alice_listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(alice_listing.status(), StatusCode::OK);
        assert!(body_string(alice_listing).await.contains("alice-note.txt"));

        // Bob's own listing (his own, separately-rooted `Storage`, the
        // same isolation `files_are_isolated_between_accounts` proves at
        // the DAV layer) never shows alice's file.
        let bob_listing =
            request_as(&app, "GET", "/ui/files/", "bob", "secret2", &[], Vec::new()).await;
        assert!(!body_string(bob_listing).await.contains("alice-note.txt"));
    }

    #[tokio::test]
    async fn ui_mkdir_creates_a_folder_that_then_lists_and_can_be_browsed() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let mkdir = request_as(
            &app,
            "POST",
            "/ui/mkdir",
            "alice",
            "secret1",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"dir=%2F&name=Photos".to_vec(),
        )
        .await;
        assert_eq!(mkdir.status(), StatusCode::SEE_OTHER);

        let listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert!(body_string(listing).await.contains("Photos"));

        let browse_new_dir = request_as(
            &app,
            "GET",
            "/ui/files/Photos",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(browse_new_dir.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ui_rejects_cross_site_mutation_posts() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;
        let response = request_as(
            &app,
            "POST",
            "/ui/mkdir",
            "alice",
            "secret1",
            &[
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Sec-Fetch-Site", "cross-site"),
            ],
            b"dir=%2F&name=forbidden".to_vec(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert!(!body_string(listing).await.contains("forbidden"));
    }

    #[tokio::test]
    async fn ui_delete_soft_deletes_through_the_real_trash_and_disappears_from_the_listing() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;
        upload_via_ui(&app, "alice", "secret1", "/", "throwaway.txt", b"bye").await;

        let delete = request_as(
            &app,
            "POST",
            "/ui/delete",
            "alice",
            "secret1",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            b"path=%2Fthrowaway.txt".to_vec(),
        )
        .await;
        assert_eq!(delete.status(), StatusCode::SEE_OTHER);

        let listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert!(!body_string(listing).await.contains("throwaway.txt"));

        // Not actually destroyed -- `/ui/delete` calls straight into
        // `breadcloud_webdav::handlers::delete::handle`, which moves it to
        // trash (CLOUD-5), verified here through that same DAV surface.
        let trash = request_as(
            &app,
            "PROPFIND",
            "/remote.php/dav/trashbin/alice/",
            "alice",
            "secret1",
            &[("Depth", "1")],
            Vec::new(),
        )
        .await;
        assert!(body_string(trash).await.contains("throwaway.txt"));
    }

    #[tokio::test]
    async fn ui_upload_over_quota_is_rejected_and_does_not_land_on_disk() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 10)]).await;

        let upload = upload_via_ui(
            &app,
            "alice",
            "secret1",
            "/",
            "big.bin",
            b"this payload is way more than ten bytes",
        )
        .await;
        assert_eq!(upload.status(), StatusCode::from_u16(507).unwrap());

        let get = request_as(&app, "GET", "/big.bin", "alice", "secret1", &[], Vec::new()).await;
        assert_eq!(get.status(), StatusCode::NOT_FOUND);
    }

    /// Regression test for the CLOUD-12 review's second finding: axum
    /// applies its own 2 MiB default body-size limit to any request read
    /// through a `DefaultBodyLimit`-aware extractor, and
    /// `/ui/upload`'s `Multipart::from_request` is exactly such an
    /// extractor -- before the combined router's `DefaultBodyLimit` layer
    /// was raised to match `MAX_UPLOAD_BYTES`, this exact shape of
    /// request (comfortably over 2 MiB, comfortably under both the real
    /// cap and the account's quota) failed with a generic 400, live-
    /// verified by the reviewer via bisection at exactly the 2 MiB
    /// boundary. The prior quota test above uses a 10-byte quota and a
    /// payload nowhere near 2 MiB, so it could not have caught this; this
    /// test exists specifically because that one does not exercise a
    /// realistic size.
    #[tokio::test]
    async fn ui_upload_larger_than_axum_default_body_limit_succeeds() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", GIB)]).await;

        // 3 MiB: well past axum's 2,097,152-byte default and the exact
        // boundary the reviewer bisected at, nowhere near the real 20 GiB
        // cap or this account's 1 GiB quota.
        let content = vec![b'x'; 3 * 1024 * 1024];
        let upload =
            upload_via_ui(&app, "alice", "secret1", "/", "big-but-legit.bin", &content).await;
        assert_eq!(
            upload.status(),
            StatusCode::SEE_OTHER,
            "a {}-byte upload, well under both MAX_UPLOAD_BYTES and the account's quota, \
             must succeed rather than being silently capped at axum's 2 MiB default",
            content.len()
        );

        let get = request_as(
            &app,
            "GET",
            "/big-but-legit.bin",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(
            get.into_body().collect().await.unwrap().to_bytes().len(),
            content.len()
        );
    }

    /// The quota-rejection path (507, nothing lands on disk) also holds
    /// at a realistic upload size, not just the tiny few-bytes scale the
    /// test above this one uses -- closing the exact gap the CLOUD-12
    /// review called out ("the integration test's over-quota case almost
    /// certainly only exercised a small file under a small quota").
    #[tokio::test]
    async fn ui_upload_over_quota_at_realistic_scale_is_rejected_and_does_not_land_on_disk() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 1024 * 1024)]).await;

        let content = vec![b'x'; 3 * 1024 * 1024];
        let upload = upload_via_ui(
            &app,
            "alice",
            "secret1",
            "/",
            "too-big-for-quota.bin",
            &content,
        )
        .await;
        assert_eq!(upload.status(), StatusCode::from_u16(507).unwrap());

        let get = request_as(
            &app,
            "GET",
            "/too-big-for-quota.bin",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(get.status(), StatusCode::NOT_FOUND);
    }

    /// Regression test for the CLOUD-12 review's first finding (stored
    /// XSS via an unescaped `href` attribute): the exact payload the
    /// reviewer used in their live repro against a running instance. The
    /// filename contains the *text* `&quot;` (six literal characters:
    /// `&`, `q`, `u`, `o`, `t`, `;`), not an actual `"` byte -- a naive
    /// filesystem-safe-filename check would wave it through, and a
    /// browser decodes that text back into a literal quote when it
    /// parses the rendered HTML, which is exactly what let the injected
    /// `<img onerror=...>` close the `href="..."` attribute and the
    /// `<a>` tag early and execute unprompted before this fix.
    #[tokio::test]
    async fn ui_files_listing_html_escapes_a_filename_that_attempts_stored_xss() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let payload = "pwn3&quot;><img src=x onerror=alert(document.cookie)>.txt";
        let upload = upload_via_ui(&app, "alice", "secret1", "/", payload, b"gotcha").await;
        assert_eq!(upload.status(), StatusCode::SEE_OTHER);

        let listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(listing.status(), StatusCode::OK);
        let html = body_string(listing).await;

        // The attribute-breakout and the live `<img>` tag must never
        // appear unescaped in the rendered page.
        assert!(
            !html.contains("\"><img"),
            "unescaped attribute break-out survived into the listing: {html}"
        );
        assert!(
            !html.contains("<img src=x onerror="),
            "the injected tag rendered as a live element: {html}"
        );

        // And the properly-escaped forms must be present, proving this is
        // escaping the value, not merely stripping or rejecting it.
        assert!(
            html.contains("&amp;quot;"),
            "expected the literal &quot; text itself to be escaped: {html}"
        );
        assert!(
            html.contains("&lt;img"),
            "expected the injected tag's `<` to be escaped: {html}"
        );
    }

    /// Same bug, same fix, in `breadcrumbs()`'s directory-name links
    /// (`template.rs`) rather than `files.rs`'s file-row links -- the
    /// CLOUD-12 review flagged both sites by name.
    #[tokio::test]
    async fn ui_breadcrumbs_html_escapes_a_directory_name_that_attempts_stored_xss() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        // Same attack shape as the file-listing payload above (an
        // HTML-entity-encoded quote plus a raw `<img>` tag), applied to a
        // directory name. No `/` or `=`, so the name is valid for
        // `mkdir` (which only rejects an embedded `/`) and the percent-
        // encoding needed to carry it through a form body and a URL path
        // segment in this test stays a fixed, known set of characters.
        let payload = "x&quot;><img onerror.alert(1)>";
        let encoded = percent_encode_test_payload(payload);

        let mkdir = request_as(
            &app,
            "POST",
            "/ui/mkdir",
            "alice",
            "secret1",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            format!("dir=%2F&name={encoded}").into_bytes(),
        )
        .await;
        assert_eq!(mkdir.status(), StatusCode::SEE_OTHER);

        let browse = request_as(
            &app,
            "GET",
            &format!("/ui/files/{encoded}"),
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(browse.status(), StatusCode::OK);
        let html = body_string(browse).await;

        assert!(
            !html.contains("\"><img"),
            "unescaped attribute break-out survived into the breadcrumbs: {html}"
        );
        assert!(
            !html.contains("<img onerror"),
            "the injected tag rendered as a live element in the breadcrumbs: {html}"
        );
        assert!(
            html.contains("&amp;quot;"),
            "expected the literal &quot; text itself to be escaped: {html}"
        );
        assert!(
            html.contains("&lt;img"),
            "expected the injected tag's `<` to be escaped: {html}"
        );
    }

    /// Percent-encode the fixed, known set of characters the two XSS
    /// regression tests' payloads use, so they can be carried unmangled
    /// through both an `application/x-www-form-urlencoded` body (where
    /// `&`/`=` are field delimiters) and a URL path segment (where `<`/
    /// `>`/space are not valid raw bytes) -- a small fixed replace chain
    /// rather than pulling in the `percent-encoding` crate as a dev-
    /// dependency of this binary crate for two tests.
    fn percent_encode_test_payload(input: &str) -> String {
        input
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('&', "%26")
            .replace('<', "%3C")
            .replace('>', "%3E")
            .replace('(', "%28")
            .replace(')', "%29")
    }

    #[tokio::test]
    async fn ui_calendar_month_view_shows_events_in_that_month_and_stays_isolated_per_account() {
        let (_dirs, app) = multi_user_test_app(&[
            ("alice", "secret1", 100 * GIB),
            ("bob", "secret2", 100 * GIB),
        ])
        .await;

        let ics = b"BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:standup-1\r\nDTSTART:20260115T090000Z\r\nDTEND:20260115T093000Z\r\nSUMMARY:Team standup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_vec();
        request_as(
            &app,
            "PUT",
            "/remote.php/dav/calendars/alice/personal/standup-1.ics",
            "alice",
            "secret1",
            &[],
            ics,
        )
        .await;

        let january = request_as(
            &app,
            "GET",
            "/ui/calendar?year=2026&month=1",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(january.status(), StatusCode::OK);
        let january_html = body_string(january).await;
        assert!(january_html.contains("Team standup"));
        assert!(january_html.contains("Personal"));

        // A different month never shows it.
        let february = request_as(
            &app,
            "GET",
            "/ui/calendar?year=2026&month=2",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert!(!body_string(february).await.contains("Team standup"));

        // Bob's own calendar view (same personal calendar id, a wholly
        // separate `CalendarStore` namespace per CLOUD-6/11) never shows
        // alice's event.
        let bob_january = request_as(
            &app,
            "GET",
            "/ui/calendar?year=2026&month=1",
            "bob",
            "secret2",
            &[],
            Vec::new(),
        )
        .await;
        assert!(!body_string(bob_january).await.contains("Team standup"));
    }

    /// CLOUD-24: a plain browser GET at the bare root (the shape a real
    /// browser navigation sends: method GET, `Accept: text/html`) redirects
    /// straight to the file browser instead of falling through to
    /// `breadcloud_webdav`'s bare directory-listing fallback.
    #[tokio::test]
    async fn browser_get_at_bare_root_redirects_to_the_file_browser() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let response = request_as(
            &app,
            "GET",
            "/",
            "alice",
            "secret1",
            &[(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )],
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers().get("Location").unwrap(),
            "/ui/files/",
            "a browser landing on the bare root must be sent to the file browser"
        );
    }

    /// The same redirect must never fire for a request that does not look
    /// like a browser navigation -- this is exactly the regression this
    /// fix must not introduce: a WebDAV/sync client's bare GET (no
    /// `Accept: text/html`, the shape every real client this codebase has
    /// been tested against actually sends) keeps getting
    /// `breadcloud_webdav`'s own bare directory-listing response.
    #[tokio::test]
    async fn non_browser_get_at_bare_root_is_not_redirected() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let response = request_as(&app, "GET", "/", "alice", "secret1", &[], Vec::new()).await;
        assert_ne!(
            response.status(),
            StatusCode::FOUND,
            "a bare GET with no Accept: text/html must not be redirected"
        );

        let response = request_as(
            &app,
            "GET",
            "/",
            "alice",
            "secret1",
            &[("Accept", "*/*")],
            Vec::new(),
        )
        .await;
        assert_ne!(
            response.status(),
            StatusCode::FOUND,
            "Accept: */* (what curl and most WebDAV clients send) must not be redirected"
        );
    }

    /// CLOUD-24's whole point is that this redirect must never regress
    /// PROPFIND discovery at the bare root (CLOUD-6/7): re-asserts the
    /// existing `current-user-principal` splice still fires, this time
    /// with a browser-shaped `Accept: text/html` header attached, to prove
    /// the new check really does gate on method, not just on the header.
    #[tokio::test]
    async fn propfind_at_bare_root_is_unaffected_even_with_a_browser_accept_header() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let response = request_as(
            &app,
            "PROPFIND",
            "/",
            "alice",
            "secret1",
            &[("Depth", "0"), ("Accept", "text/html")],
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::from_u16(207).unwrap());
        let xml = body_string(response).await;
        assert!(
            xml.contains("<d:current-user-principal>"),
            "expected current-user-principal in bare-root PROPFIND response even with a \
             browser-shaped Accept header, got: {xml}"
        );
    }

    /// OPTIONS at the bare root (a real WebDAV client's capability probe)
    /// is also untouched: different method, so `is_browser_root_navigation`
    /// never matches regardless of headers.
    #[tokio::test]
    async fn options_at_bare_root_is_unaffected() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let response = request_as(
            &app,
            "OPTIONS",
            "/",
            "alice",
            "secret1",
            &[("Accept", "text/html")],
            Vec::new(),
        )
        .await;
        assert_ne!(response.status(), StatusCode::FOUND);
    }

    /// The web UI's shared stylesheet is served at `/ui/style.css`, and
    /// every rendered page links to it (rather than repeating an inline
    /// `<style>` block per response).
    #[tokio::test]
    async fn ui_stylesheet_is_served_and_linked_from_every_page() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;

        let css = request_as(
            &app,
            "GET",
            "/ui/style.css",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(css.status(), StatusCode::OK);
        let content_type = css
            .headers()
            .get("Content-Type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(content_type.contains("text/css"));
        let css_body = body_string(css).await;
        assert!(!css_body.trim().is_empty());
        assert!(css_body.contains("--bg"));

        let home = request_as(&app, "GET", "/ui/", "alice", "secret1", &[], Vec::new()).await;
        let home_html = body_string(home).await;
        assert!(home_html.contains(r#"<link rel="stylesheet" href="/ui/style.css">"#));
        assert!(!home_html.contains("<style>"));

        let files = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        let files_html = body_string(files).await;
        assert!(files_html.contains(r#"<link rel="stylesheet" href="/ui/style.css">"#));
    }

    /// The quota bar's color reflects the same 80/90/100 percent bands
    /// `breadcloud-events`' `crossed_threshold` (CLOUD-13) already warns
    /// on -- presentation only, no event involved here, just proving the
    /// file listing actually picks a `warn`/`critical` class rather than
    /// always rendering a plain, uncolored bar.
    #[tokio::test]
    async fn ui_quota_bar_gets_a_warn_class_approaching_the_limit() {
        let (_dirs, app) = multi_user_test_app(&[("alice", "secret1", 100)]).await;

        let upload =
            upload_via_ui(&app, "alice", "secret1", "/", "eighty-five.bin", &[0u8; 85]).await;
        assert_eq!(upload.status(), StatusCode::SEE_OTHER);

        let listing = request_as(
            &app,
            "GET",
            "/ui/files/",
            "alice",
            "secret1",
            &[],
            Vec::new(),
        )
        .await;
        let html = body_string(listing).await;
        assert!(
            html.contains(r#"class="quota-bar warn""#),
            "expected the quota bar to carry the warn class at 85% of a 100-byte quota: {html}"
        );
    }
    struct ObservedBodyReader {
        reader: tokio::io::DuplexStream,
        started: Option<tokio::sync::oneshot::Sender<()>>,
    }

    impl tokio::io::AsyncRead for ObservedBodyReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            std::pin::Pin::new(&mut self.reader).poll_read(cx, buffer)
        }
    }

    async fn simultaneous_conditional_puts(
        app: &Router,
        uri: &str,
        condition: (&str, &str),
        first: Vec<u8>,
        second: Vec<u8>,
    ) -> (Response, Response) {
        use tokio::io::AsyncWriteExt;
        let mut writers = Vec::new();
        let mut tasks = Vec::new();
        let mut starts = Vec::new();
        for _ in 0..2 {
            let (writer, reader) = tokio::io::duplex(4096);
            let (start, started) = tokio::sync::oneshot::channel();
            let body = Body::from_stream(tokio_util::io::ReaderStream::new(ObservedBodyReader {
                reader,
                started: Some(start),
            }));
            let request = Request::builder()
                .method("PUT")
                .uri(uri)
                .header("Authorization", basic_auth("alice", "secret1"))
                .header(condition.0, condition.1)
                .body(body)
                .unwrap();
            let app = app.clone();
            tasks.push(tokio::spawn(
                async move { app.oneshot(request).await.unwrap() },
            ));
            starts.push(started);
            writers.push(writer);
        }
        // Both handlers reached body reading before either receives content.
        // Checking preconditions before that await recreates the lost-update
        // bug deterministically; checking under the shared gate avoids it.
        for started in starts {
            tokio::time::timeout(std::time::Duration::from_secs(5), started)
                .await
                .unwrap()
                .unwrap();
        }
        writers[0].write_all(&first).await.unwrap();
        writers[1].write_all(&second).await.unwrap();
        drop(writers);
        let second = tasks.pop().unwrap();
        let first = tasks.pop().unwrap();
        (first.await.unwrap(), second.await.unwrap())
    }

    #[tokio::test]
    async fn conditional_calendar_and_contact_mutations_are_atomic_across_requests() {
        let (dirs, app) = multi_user_test_app(&[("alice", "secret1", 100 * GIB)]).await;
        for (uri, template, calendar) in [
            ("/remote.php/dav/calendars/alice/personal/concurrent.ics", "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:concurrent\r\nDTSTART:20260401T090000Z\r\nSUMMARY:VALUE\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", true),
            ("/remote.php/dav/addressbooks/alice/contacts/concurrent.vcf", "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:concurrent\r\nFN:VALUE\r\nEND:VCARD\r\n", false),
        ] {
            let initial = template.replace("VALUE", "initial").into_bytes();
            assert_eq!(request_as(&app, "PUT", uri, "alice", "secret1", &[], initial).await.status(), StatusCode::CREATED);
            let read = request_as(&app, "GET", uri, "alice", "secret1", &[], vec![]).await;
            let etag = read.headers().get("ETag").unwrap().to_str().unwrap().to_string();
            let first = template.replace("VALUE", "first").into_bytes();
            let second = template.replace("VALUE", "second").into_bytes();
            let (a, b) = simultaneous_conditional_puts(&app, uri, ("If-Match", &etag), first.clone(), second.clone()).await;
            assert!(matches!((a.status(), b.status()), (StatusCode::NO_CONTENT, StatusCode::PRECONDITION_FAILED) | (StatusCode::PRECONDITION_FAILED, StatusCode::NO_CONTENT)));
            let read = request_as(&app, "GET", uri, "alice", "secret1", &[], vec![]).await;
            let content = read.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(content.as_ref(), if a.status() == StatusCode::NO_CONTENT { first.as_slice() } else { second.as_slice() });
            // The losing device's old ETag must also fail to delete the winner.
            assert_eq!(request_as(&app, "DELETE", uri, "alice", "secret1", &[("If-Match", &etag)], vec![]).await.status(), StatusCode::PRECONDITION_FAILED);
            assert_eq!(request_as(&app, "GET", uri, "alice", "secret1", &[], vec![]).await.status(), StatusCode::OK);
            let create_uri = uri.replace("concurrent", "create-race");
            let (a, b) = simultaneous_conditional_puts(&app, &create_uri, ("If-None-Match", "*"), first, second).await;
            assert!(matches!((a.status(), b.status()), (StatusCode::CREATED, StatusCode::PRECONDITION_FAILED) | (StatusCode::PRECONDITION_FAILED, StatusCode::CREATED)));
            let ctag = if calendar { CalendarStore::new(dirs._calendars.path()).ctag("alice", "personal").await.unwrap() } else { AddressbookStore::new(dirs._addressbooks.path()).ctag("alice", "contacts").await.unwrap() };
            assert_eq!(ctag, "3", "only initial creation, winning update, and winning create record changes");
        }
    }
}
