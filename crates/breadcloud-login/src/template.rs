//! The browser-facing pages: the login form itself, its "wrong
//! credentials" re-render, the success page, and a generic "this link is
//! no longer valid" page for an unknown/expired/already-used flow.
//!
//! **Standalone styling, not `breadcloud-webui`'s shared stylesheet.**
//! CLOUD-24's `/ui/style.css` sits behind the same CLOUD-11 Basic-auth
//! gate every other `/ui/*` page does (see that crate's module doc): a
//! browser that has not authenticated yet -- which, for this exact page,
//! is *every* visitor, since authenticating via this form is the whole
//! point -- would get a spurious native Basic-auth prompt just to fetch
//! the CSS, before ever seeing the login form itself. Rather than carve a
//! one-off unauthenticated exception into that gate for a single static
//! asset, this page carries a small inline `<style>` block that mirrors
//! `breadcloud-webui/src/style.css`'s dark palette (same CSS custom
//! property values) for visual consistency with the rest of the
//! deployment, without an HTTP dependency on that crate's auth-gated
//! route. Same "no build step, no framework" ethos as that stylesheet,
//! just not literally shared -- documented here since it is a deliberate
//! deviation from the pattern the issue suggested trying first.
//!
//! Every value interpolated from request- or user-supplied data
//! ([`login_page`]'s `error`, [`success_page`]'s `login_name`) goes
//! through [`html_escape`] first, the same discipline
//! `breadcloud-webui::template::html_escape` documents and CLOUD-12's
//! review enforced there.

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;

const STYLE: &str = r#"
:root {
  color-scheme: dark;
  --bg: #16171b;
  --bg-elevated: #1e2025;
  --border: #33363d;
  --text: #e8e9eb;
  --text-dim: #a1a5ad;
  --accent: #5b9dff;
  --accent-hover: #7fb2ff;
  --danger: #ff6b6b;
  --danger-bg: rgba(255, 107, 107, 0.14);
  --radius: 8px;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  min-height: 100vh;
  display: flex;
  align-items: center;
  justify-content: center;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  font-size: 15px;
  line-height: 1.5;
  color: var(--text);
  background: var(--bg);
  -webkit-font-smoothing: antialiased;
}
.card {
  width: 100%;
  max-width: 360px;
  margin: 24px;
  padding: 28px;
  background: var(--bg-elevated);
  border: 1px solid var(--border);
  border-radius: var(--radius);
}
h1 { margin: 0 0 4px; font-size: 20px; }
p.sub { margin: 0 0 20px; color: var(--text-dim); font-size: 13.5px; }
label { display: block; margin: 14px 0 6px; font-size: 13.5px; color: var(--text-dim); }
input[type="text"], input[type="password"] {
  width: 100%;
  padding: 9px 10px;
  background: var(--bg);
  border: 1px solid var(--border);
  border-radius: 6px;
  color: var(--text);
  font-size: 15px;
}
input:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
button {
  width: 100%;
  margin-top: 20px;
  padding: 10px;
  background: var(--accent);
  border: none;
  border-radius: 6px;
  color: #0b1420;
  font-size: 15px;
  font-weight: 600;
  cursor: pointer;
}
button:hover { background: var(--accent-hover); }
.error {
  margin: 0 0 14px;
  padding: 10px 12px;
  background: var(--danger-bg);
  border: 1px solid var(--danger);
  border-radius: 6px;
  color: var(--danger);
  font-size: 13.5px;
}
"#;

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n{body}\n</body>\n</html>\n",
    )
}

/// `X-Frame-Options`/`Content-Security-Policy: frame-ancestors 'none'` on
/// every response this crate renders: a page whose only job is to collect
/// a real account password and grant access should not be embeddable in a
/// third-party `<iframe>` at all (clickjacking hardening, complementary to
/// -- not a substitute for -- the CSRF token check in `handlers.rs`, which
/// defends the POST itself rather than the visual framing of the GET).
fn html_response(status: StatusCode, body: String) -> Response {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("X-Frame-Options", "DENY")
        .header("Content-Security-Policy", "frame-ancestors 'none'")
        .body(Body::from(body))
        .unwrap()
}

/// The login form itself (`GET /login/v2/flow/{token}`, and the
/// re-rendered version of the same page after a failed submission). `csrf`
/// is embedded as a hidden field and must come back unchanged on submit
/// (see `flow.rs`'s module doc for why a fixed, non-rotating token is
/// enough here). `error`, when present, is a fixed, non-user-controlled
/// string this crate chose (e.g. "incorrect username or password") --
/// still escaped, on the same "never trust an interpolation site just
/// because you wrote the string today" discipline as everything else here.
pub fn login_page(login_token: &str, csrf: &str, error: Option<&str>) -> Response {
    let error_html = error
        .map(|e| format!("<p class=\"error\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        r#"<div class="card">
<h1>Sign in</h1>
<p class="sub">Approve this app's access to your breadcloud account.</p>
{error_html}
<form method="post" action="/login/v2/flow/{token}">
<input type="hidden" name="csrf_token" value="{csrf}">
<label for="username">Username</label>
<input type="text" id="username" name="username" autocomplete="username" autofocus required>
<label for="password">Password</label>
<input type="password" id="password" name="password" autocomplete="current-password" required>
<button type="submit">Sign in &amp; approve</button>
</form>
</div>"#,
        token = html_escape(login_token),
        csrf = html_escape(csrf),
    );
    html_response(StatusCode::OK, page("Sign in - breadcloud", &body))
}

/// Shown once a flow is successfully granted -- matches real Nextcloud's
/// own login-v2 success page in spirit ("you can close this window now"),
/// per the issue's own framing.
pub fn success_page(login_name: &str) -> Response {
    let body = format!(
        r#"<div class="card">
<h1>Signed in</h1>
<p class="sub">Access approved for <strong>{name}</strong>. You can close this window now.</p>
</div>"#,
        name = html_escape(login_name)
    );
    html_response(StatusCode::OK, page("Signed in - breadcloud", &body))
}

/// An unknown, expired, or already-completed flow -- the one page shown
/// for all three (see `flow::FormViewError`'s doc comment for why those
/// are deliberately not distinguished).
pub fn expired_or_unknown_page() -> Response {
    let body = r#"<div class="card">
<h1>Link no longer valid</h1>
<p class="sub">This sign-in link has expired or was already used. Start the sign-in process again from the app that opened it.</p>
</div>"#;
    html_response(
        StatusCode::NOT_FOUND,
        page("Link expired - breadcloud", body),
    )
}

/// A submitted CSRF token did not match -- kept distinct from
/// [`expired_or_unknown_page`] (403, not 404) since this is a genuinely
/// different failure (a real, still-pending flow whose submission looked
/// forged), not "this flow does not exist".
pub fn csrf_rejected_page() -> Response {
    let body = r#"<div class="card">
<h1>Request rejected</h1>
<p class="sub">This submission could not be verified. Go back and try again from the original sign-in link.</p>
</div>"#;
    html_response(
        StatusCode::FORBIDDEN,
        page("Request rejected - breadcloud", body),
    )
}
