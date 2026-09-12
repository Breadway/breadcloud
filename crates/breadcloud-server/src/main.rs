//! breadcloud WebDAV server: wires up `breadcloud-storage` and
//! `breadcloud-webdav` into a runnable binary. See the workspace README
//! for configuration and how to point it at a real file tree.

use std::net::SocketAddr;
use std::path::PathBuf;

use breadcloud_storage::Storage;
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

    let bind = std::env::var("BREADCLOUD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind
        .parse()
        .expect("BREADCLOUD_BIND must be a valid address, e.g. 127.0.0.1:8080");

    let storage = Storage::new(root.clone());
    let uploads = Storage::new(uploads_root.clone());
    let mut state = AppState::new(storage, uploads);

    let user = std::env::var("BREADCLOUD_USER").ok();
    let pass = std::env::var("BREADCLOUD_PASS").ok();
    match (user, pass) {
        (Some(u), Some(p)) => {
            tracing::info!("basic auth enabled for user {u}");
            state = state.with_auth(u, p);
        }
        _ => {
            tracing::warn!(
                "BREADCLOUD_USER/BREADCLOUD_PASS not set: server is running without authentication"
            );
        }
    }

    tracing::info!(
        "serving {} on {} (chunked-upload staging: {})",
        root.display(),
        addr,
        uploads_root.display()
    );

    let app = breadcloud_webdav::router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    axum::serve(listener, app).await.expect("server error");
}
