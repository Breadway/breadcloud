//! Wires `breadcloud-webdav`, `breadcloud-caldav`, and `breadcloud-carddav`
//! into one axum app on one port, under one shared auth gate, instead of
//! running three HTTP servers for what is (as far as a client's TCP
//! connection is concerned) one breadcloud instance. See the workspace
//! README's "Sharing the HTTP server with CalDAV/CardDAV" section for why
//! this lives here rather than as shared code inside any protocol crate:
//! the composition itself -- "which protocol owns this path, and what
//! credentials gate all three of them" -- is a property of *this binary's*
//! configuration (every crate's `AppState.auth` is always built from the
//! same `BREADCLOUD_USER`/`BREADCLOUD_PASS` pair below), not of any crate
//! on its own.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use breadcloud_webdav::auth::check_basic_auth;

#[derive(Clone)]
struct CombinedState {
    webdav: breadcloud_webdav::AppState,
    caldav: breadcloud_caldav::AppState,
    carddav: breadcloud_carddav::AppState,
}

/// Build the combined app. Deliberately does not call any crate's own
/// `router()` (each of which layers its own independent auth middleware,
/// meant for that crate's standalone use and its own test suite): doing
/// that here would check credentials multiple times per request and,
/// per the CLOUD-6 implementation notes, composing routers that each only
/// use a top-level `.fallback()` does not work cleanly with axum's
/// routing. Instead, this calls each crate's plain `dispatch(state, req)`
/// function directly from one shared fallback, gated by one shared auth
/// middleware.
pub fn router(
    webdav: breadcloud_webdav::AppState,
    caldav: breadcloud_caldav::AppState,
    carddav: breadcloud_carddav::AppState,
) -> Router {
    let state = CombinedState {
        webdav,
        caldav,
        carddav,
    };
    Router::new()
        .fallback(dispatch)
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, auth_gate))
}

async fn dispatch(State(state): State<CombinedState>, req: Request) -> Response {
    let path = req.uri().path().to_string();

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
        let response = breadcloud_caldav::dispatch(state.caldav, req).await;
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
        return breadcloud_carddav::dispatch(state.carddav, req).await;
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
    // output.
    let want_principal = req.method().as_str() == "PROPFIND" && path == "/";
    let principal_href = want_principal.then(|| breadcloud_caldav::principal_href(&state.caldav));
    let response = breadcloud_webdav::dispatch(state.webdav, req).await;

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

async fn auth_gate(State(state): State<CombinedState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();

    // The same "first request, before the client has credentials to
    // send" exemptions `breadcloud_webdav::auth` (status.php) and each
    // protocol crate's own standalone router (`.well-known/caldav`,
    // `.well-known/carddav`) carve out individually -- applied once here,
    // since this combined app bypasses every crate's own auth layer in
    // favor of this single shared gate.
    if req.method() == Method::GET && path.ends_with("/status.php") {
        return next.run(req).await;
    }
    if path == "/.well-known/caldav" || path == "/.well-known/carddav" {
        return next.run(req).await;
    }

    let Some(auth) = &state.webdav.auth else {
        return next.run(req).await;
    };

    if check_basic_auth(req.headers(), auth) {
        next.run(req).await
    } else {
        unauthorized()
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

#[cfg(test)]
mod tests {
    use super::*;
    use breadcloud_storage::{AddressbookStore, CalendarStore, Storage, TrashStore, VersionStore};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

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
        let webdav = breadcloud_webdav::AppState::new(storage, uploads, versions, trash);

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
        (dirs, router(webdav, caldav, carddav))
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
        assert!(
            xml.contains(
                "<d:current-user-principal><d:href>/remote.php/dav/principals/anonymous/</d:href></d:current-user-principal>"
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
        let storage_dir = TempDir::new().unwrap();
        let uploads_dir = TempDir::new().unwrap();
        let versions_dir = TempDir::new().unwrap();
        let trash_dir = TempDir::new().unwrap();
        let calendars_dir = TempDir::new().unwrap();
        let addressbooks_dir = TempDir::new().unwrap();
        let webdav = breadcloud_webdav::AppState::new(
            Storage::new(storage_dir.path()),
            Storage::new(uploads_dir.path()),
            VersionStore::new(versions_dir.path()),
            TrashStore::new(trash_dir.path()),
        )
        .with_auth("dev", "secret");
        let caldav = breadcloud_caldav::AppState::new(CalendarStore::new(calendars_dir.path()))
            .with_auth("dev", "secret");
        let carddav =
            breadcloud_carddav::AppState::new(AddressbookStore::new(addressbooks_dir.path()))
                .with_auth("dev", "secret");
        let app = router(webdav, caldav, carddav);

        let req = Request::builder()
            .method("GET")
            .uri("/.well-known/carddav")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    }
}
