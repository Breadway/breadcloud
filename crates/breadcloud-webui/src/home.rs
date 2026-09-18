//! `/ui` and `/ui/`: a tiny landing page linking to the two real
//! sections (files, calendar). Nextcloud's own dashboard/activity
//! feed/notification center is explicitly out of scope for this issue --
//! see the workspace README's CLOUD-12 section -- so this is genuinely
//! just a signpost, not a third page's worth of content.

use axum::response::Response;

use crate::template::{html_response, page};

pub fn handle(user: &str) -> Response {
    let body = r#"<h1>breadcloud</h1>
<p><a href="/ui/files/">Browse your files</a></p>
<p><a href="/ui/calendar">View your calendar</a></p>"#;
    html_response(page("breadcloud", user, "", body))
}
