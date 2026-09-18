//! breadcloud WebDAV + CalDAV server: wires up `breadcloud-storage`,
//! `breadcloud-webdav`, and `breadcloud-caldav` into a runnable binary,
//! all served on one port (see `combined`). See the workspace README for
//! configuration and how to point it at a real file tree / calendar set.

mod combined;

use std::net::SocketAddr;
use std::path::PathBuf;

use breadcloud_storage::{CalendarStore, Storage, TrashStore, VersionStore};
use breadcloud_webdav::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let root = std::env::var("BREADCLOUD_ROOT").unwrap_or_else(|_| "./data".to_string());
    let root = PathBuf::from(root);
    if !root.exists() {
        std::fs::create_dir_all(&root).expect("failed to create storage root");
    }
    let root = root.canonicalize().expect("failed to resolve storage root");

    // Staging area for in-progress chunked uploads (see
    // breadcloud_webdav::handlers::uploads). Kept out of BREADCLOUD_ROOT
    // entirely -- not just dot-hidden inside it -- so a PROPFIND of the
    // real tree can never see someone's in-flight upload chunks.
    let uploads_root = std::env::var("BREADCLOUD_UPLOADS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("breadcloud-uploads"));
    if !uploads_root.exists() {
        std::fs::create_dir_all(&uploads_root).expect("failed to create uploads staging root");
    }
    let uploads_root = uploads_root
        .canonicalize()
        .expect("failed to resolve uploads staging root");

    // Per-file version history (CLOUD-4). Kept outside BREADCLOUD_ROOT,
    // same reasoning as the uploads staging area above: a PROPFIND of the
    // real tree must never see it. Unlike uploads staging this needs to
    // persist across restarts, so it defaults to a sibling directory of
    // the data root rather than the system temp dir.
    let versions_root = std::env::var("BREADCLOUD_VERSIONS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./breadcloud-versions"));
    if !versions_root.exists() {
        std::fs::create_dir_all(&versions_root).expect("failed to create version history root");
    }
    let versions_root = versions_root
        .canonicalize()
        .expect("failed to resolve version history root");

    // Soft-delete trash (CLOUD-5). Kept outside BREADCLOUD_ROOT, same
    // reasoning as uploads staging and version history above, and (like
    // version history) needs to persist across restarts, so it also
    // defaults to a sibling directory of the data root rather than the
    // system temp dir.
    let trash_root = std::env::var("BREADCLOUD_TRASH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./breadcloud-trash"));
    if !trash_root.exists() {
        std::fs::create_dir_all(&trash_root).expect("failed to create trash root");
    }
    let trash_root = trash_root
        .canonicalize()
        .expect("failed to resolve trash root");

    // Calendar storage (CLOUD-6). Its own root, same reasoning as
    // uploads/versions/trash: outside BREADCLOUD_ROOT so a PROPFIND of
    // the file tree can never see it, and (like versions/trash, unlike
    // uploads staging) a sibling directory by default since it needs to
    // persist across restarts.
    let calendars_root = std::env::var("BREADCLOUD_CALENDARS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./breadcloud-calendars"));
    if !calendars_root.exists() {
        std::fs::create_dir_all(&calendars_root).expect("failed to create calendars root");
    }
    let calendars_root = calendars_root
        .canonicalize()
        .expect("failed to resolve calendars root");

    let bind = std::env::var("BREADCLOUD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind
        .parse()
        .expect("BREADCLOUD_BIND must be a valid address, e.g. 127.0.0.1:8080");

    let storage = Storage::new(root.clone());
    let uploads = Storage::new(uploads_root.clone());
    let versions = VersionStore::new(versions_root.clone());
    let trash = TrashStore::new(trash_root.clone());
    let mut webdav_state = AppState::new(storage, uploads, versions, trash);

    let calendars = CalendarStore::new(calendars_root.clone());
    let mut caldav_state = breadcloud_caldav::AppState::new(calendars.clone());

    // The calendar owner: there is only one real, placeholder
    // owner-equivalent user until CLOUD-11 (same stance
    // breadcloud_webdav::auth takes), so this is also the name calendars
    // get bootstrapped under below, not derived per-request from
    // anything. Falls back to "dev" (matching the workspace README's own
    // local-dev examples) rather than refusing to start when
    // BREADCLOUD_USER is unset, since an open local server still needs
    // some directory name to create calendars under.
    let user = std::env::var("BREADCLOUD_USER").ok();
    let calendar_owner = user.clone().unwrap_or_else(|| "dev".to_string());
    let pass = std::env::var("BREADCLOUD_PASS").ok();
    match (user, pass) {
        (Some(u), Some(p)) => {
            tracing::info!("basic auth enabled for user {u}");
            webdav_state = webdav_state.with_auth(u.clone(), p.clone());
            caldav_state = caldav_state.with_auth(u, p);
        }
        _ => {
            tracing::warn!(
                "BREADCLOUD_USER/BREADCLOUD_PASS not set: server is running without authentication"
            );
        }
    }

    // Bootstrap the baseline calendar set (CLOUD-6 asks for at least
    // personal/family/school) if they do not already exist. Idempotent:
    // a restart against an already-populated calendars root just no-ops
    // here. Further calendars can be created at any time via MKCALENDAR.
    for (id, name, color) in [
        ("personal", "Personal", Some("#1d7dd6")),
        ("family", "Family", Some("#2fa84f")),
        ("school", "School", Some("#c9622a")),
    ] {
        calendars
            .ensure_calendar(&calendar_owner, id, name, color)
            .await
            .unwrap_or_else(|e| panic!("failed to bootstrap calendar {id}: {e}"));
    }

    tracing::info!(
        "serving {} on {} (chunked-upload staging: {}, version history: {}, trash: {}, calendars: {})",
        root.display(),
        addr,
        uploads_root.display(),
        versions_root.display(),
        trash_root.display(),
        calendars_root.display()
    );

    let app = combined::router(webdav_state, caldav_state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    axum::serve(listener, app).await.expect("server error");
}
