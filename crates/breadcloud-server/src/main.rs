//! breadcloud WebDAV + CalDAV + CardDAV server: wires up
//! `breadcloud-storage`, `breadcloud-webdav`, `breadcloud-caldav`, and
//! `breadcloud-carddav` into a runnable binary, all served on one port
//! (see `combined`). See the workspace README for configuration and how
//! to point it at a real file tree / calendar / addressbook set.

mod combined;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use breadcloud_storage::{AddressbookStore, CalendarStore, Storage, TrashStore, VersionStore};
use breadcloud_webdav::{Account, AccountStore, AppState};

/// 1 GiB, in bytes. Account quotas (`BREADCLOUD_ACCOUNT_<n>_QUOTA_GB`, and
/// [`DEFAULT_LEGACY_QUOTA_GIB`]) are specified in whole GiB, matching the
/// binary-unit convention this codebase's other size constants already use
/// (`breadcloud_storage::{DEFAULT_TRASH_MAX_TOTAL_BYTES, DEFAULT_MAX_VERSION_SET_BYTES}`).
const GIB: u64 = 1024 * 1024 * 1024;

/// Quota given to the single account the legacy `BREADCLOUD_USER`/
/// `BREADCLOUD_PASS` fallback creates (see `load_accounts`). Arbitrary in
/// the sense that nothing in the issue names a specific figure for a
/// single-account deployment (the 100/100/50/10/100 GB mapping is for the
/// five-account case) -- 100 GiB matches the most common tier among those
/// five, which is as reasonable a default as any for the one-account
/// fallback this exists to keep working unchanged.
const DEFAULT_LEGACY_QUOTA_GIB: u64 = 100;

/// The account configuration this server ended up with, however it was
/// derived (see `load_accounts`), plus enough to know how each account's
/// storage should be laid out on disk.
struct LoadedAccounts {
    /// Real, independently-authenticated accounts. Empty means "open
    /// mode" -- no accounts configured at all, the server runs
    /// unauthenticated (unchanged from before CLOUD-11).
    accounts: Vec<Account>,
    /// Username -> quota in bytes, for every entry in `accounts`.
    quota_by_user: HashMap<String, u64>,
    /// Whether each account's file/upload/version/trash storage should
    /// live in its own subdirectory of the configured base roots (`true`,
    /// the real multi-account case) or directly at the base root itself
    /// (`false`): the legacy single-account fallback and open mode both
    /// use `false` so an existing single-user deployment's on-disk layout
    /// -- data directly under `BREADCLOUD_ROOT`, not
    /// `BREADCLOUD_ROOT/<username>` -- keeps working with zero migration.
    nest_storage_per_user: bool,
    /// The account every request operates as when `accounts` is empty.
    /// Ignored otherwise (every request must then authenticate as one of
    /// `accounts`).
    default_user: String,
}

/// Load the account set from environment variables, trying three shapes
/// in order (see the workspace README's "Multi-user accounts" section for
/// the full reasoning):
///
/// 1. **Multi-account**: `BREADCLOUD_ACCOUNT_<n>_USER` /
///    `BREADCLOUD_ACCOUNT_<n>_PASS` / `BREADCLOUD_ACCOUNT_<n>_QUOTA_GB`,
///    scanned from `n = 1` until the first missing `_USER`. This is the
///    real CLOUD-11 shape: N independently authenticated,
///    independently-quota'd, independently-storage-isolated accounts.
/// 2. **Legacy single-account**: the pre-CLOUD-11 `BREADCLOUD_USER` /
///    `BREADCLOUD_PASS` pair, used only when no `BREADCLOUD_ACCOUNT_1_USER`
///    is set. Keeps the README's "Running locally" quickstart and any
///    existing single-user deployment working unchanged, with a single
///    account at a default quota.
/// 3. **Open**: neither of the above set at all -- the server runs
///    unauthenticated, exactly as before CLOUD-11, as a single implicit
///    `"dev"` account with no quota limit.
fn load_accounts() -> LoadedAccounts {
    let mut accounts = Vec::new();
    let mut quota_by_user = HashMap::new();
    let mut n = 1u32;
    while let Ok(username) = std::env::var(format!("BREADCLOUD_ACCOUNT_{n}_USER")) {
        let password = std::env::var(format!("BREADCLOUD_ACCOUNT_{n}_PASS")).unwrap_or_else(|_| {
            panic!(
                "BREADCLOUD_ACCOUNT_{n}_USER is set but BREADCLOUD_ACCOUNT_{n}_PASS is not \
                 (every configured account needs both)"
            )
        });
        let quota_gib: u64 = std::env::var(format!("BREADCLOUD_ACCOUNT_{n}_QUOTA_GB"))
            .unwrap_or_else(|_| {
                panic!(
                    "BREADCLOUD_ACCOUNT_{n}_USER is set but BREADCLOUD_ACCOUNT_{n}_QUOTA_GB is \
                     not (every configured account needs a quota)"
                )
            })
            .parse()
            .unwrap_or_else(|_| {
                panic!("BREADCLOUD_ACCOUNT_{n}_QUOTA_GB must be a whole number of GiB")
            });
        let quota_bytes = quota_gib * GIB;
        quota_by_user.insert(username.clone(), quota_bytes);
        accounts.push(Account::new(username, &password, quota_bytes));
        n += 1;
    }

    if !accounts.is_empty() {
        let default_user = accounts[0].username.clone();
        return LoadedAccounts {
            accounts,
            quota_by_user,
            nest_storage_per_user: true,
            default_user,
        };
    }

    let legacy_user = std::env::var("BREADCLOUD_USER").ok();
    let legacy_pass = std::env::var("BREADCLOUD_PASS").ok();
    if let (Some(username), Some(password)) = (legacy_user, legacy_pass) {
        let quota_bytes = DEFAULT_LEGACY_QUOTA_GIB * GIB;
        quota_by_user.insert(username.clone(), quota_bytes);
        let default_user = username.clone();
        return LoadedAccounts {
            accounts: vec![Account::new(username, &password, quota_bytes)],
            quota_by_user,
            nest_storage_per_user: false,
            default_user,
        };
    }

    LoadedAccounts {
        accounts: Vec::new(),
        quota_by_user: HashMap::new(),
        nest_storage_per_user: false,
        default_user: "dev".to_string(),
    }
}

/// Resolve an env var to a directory path (falling back to `default` when
/// unset), creating it if it does not exist and canonicalizing the
/// result.
fn resolve_root(env_var: &str, default: &str) -> PathBuf {
    let root = std::env::var(env_var).unwrap_or_else(|_| default.to_string());
    let root = PathBuf::from(root);
    if !root.exists() {
        std::fs::create_dir_all(&root).unwrap_or_else(|e| {
            panic!(
                "failed to create {env_var} directory {}: {e}",
                root.display()
            )
        });
    }
    root.canonicalize().unwrap_or_else(|e| {
        panic!(
            "failed to resolve {env_var} directory {}: {e}",
            root.display()
        )
    })
}

/// One account's subdirectory of `base` (CLOUD-11), or `base` itself when
/// `nest` is false (the legacy single-account and open-mode cases -- see
/// `LoadedAccounts::nest_storage_per_user`). Created if missing and
/// canonicalized, same as `resolve_root`.
fn per_user_dir(base: &Path, username: &str, nest: bool) -> PathBuf {
    let dir = if nest {
        base.join(username)
    } else {
        base.to_path_buf()
    };
    if !dir.exists() {
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
            panic!("failed to create storage directory {}: {e}", dir.display())
        });
    }
    dir.canonicalize()
        .unwrap_or_else(|e| panic!("failed to resolve storage directory {}: {e}", dir.display()))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let root = resolve_root("BREADCLOUD_ROOT", "./data");

    // Staging area for in-progress chunked uploads (see
    // breadcloud_webdav::handlers::uploads). Kept out of BREADCLOUD_ROOT
    // entirely -- not just dot-hidden inside it -- so a PROPFIND of the
    // real tree can never see someone's in-flight upload chunks. Falls
    // back to the system temp dir rather than a sibling directory, unlike
    // the roots below, since it never needs to persist across restarts.
    let uploads_root = std::env::var("BREADCLOUD_UPLOADS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("breadcloud-uploads"));
    if !uploads_root.exists() {
        std::fs::create_dir_all(&uploads_root).expect("failed to create uploads staging root");
    }
    let uploads_root = uploads_root
        .canonicalize()
        .expect("failed to resolve uploads staging root");

    // Per-file version history (CLOUD-4), soft-delete trash (CLOUD-5),
    // calendars (CLOUD-6), and addressbooks (CLOUD-7): all outside
    // BREADCLOUD_ROOT for the same reason as uploads staging, and all
    // default to sibling directories (rather than the system temp dir)
    // since, unlike uploads staging, they need to persist across
    // restarts.
    let versions_root = resolve_root("BREADCLOUD_VERSIONS_ROOT", "./breadcloud-versions");
    let trash_root = resolve_root("BREADCLOUD_TRASH_ROOT", "./breadcloud-trash");
    let calendars_root = resolve_root("BREADCLOUD_CALENDARS_ROOT", "./breadcloud-calendars");
    let addressbooks_root =
        resolve_root("BREADCLOUD_ADDRESSBOOKS_ROOT", "./breadcloud-addressbooks");

    let bind = std::env::var("BREADCLOUD_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let addr: SocketAddr = bind
        .parse()
        .expect("BREADCLOUD_BIND must be a valid address, e.g. 127.0.0.1:8080");

    let loaded = load_accounts();
    let usernames: Vec<String> = if loaded.accounts.is_empty() {
        vec![loaded.default_user.clone()]
    } else {
        loaded.accounts.iter().map(|a| a.username.clone()).collect()
    };

    // One Storage/uploads-Storage/VersionStore/TrashStore per account
    // (CLOUD-11), each root-sandboxed to that account's own subdirectory:
    // `DavPath` already guarantees no request can escape a `Storage`'s
    // root via `..`, so giving each account a genuinely separate root
    // (rather than one shared root with a URL-trusted prefix, which is
    // what this server did before CLOUD-11) means one account's requests
    // are structurally incapable of reaching another's files, versions,
    // or trash, regardless of what any request claims.
    let mut webdav_by_user = HashMap::with_capacity(usernames.len());
    for username in &usernames {
        let user_root = per_user_dir(&root, username, loaded.nest_storage_per_user);
        let user_uploads_root = per_user_dir(&uploads_root, username, loaded.nest_storage_per_user);
        let user_versions_root =
            per_user_dir(&versions_root, username, loaded.nest_storage_per_user);
        let user_trash_root = per_user_dir(&trash_root, username, loaded.nest_storage_per_user);

        let storage = Storage::new(user_root);
        let uploads = Storage::new(user_uploads_root);
        let versions = VersionStore::new(user_versions_root);
        let trash = TrashStore::new(user_trash_root);

        let mut state =
            AppState::new(storage, uploads, versions, trash).with_owner(username.clone());
        if let Some(&quota_bytes) = loaded.quota_by_user.get(username) {
            state = state.with_quota_limit(quota_bytes);
        }
        webdav_by_user.insert(username.clone(), state);
    }

    // Calendars (CLOUD-6) and addressbooks (CLOUD-7) stay on one shared
    // root each, unlike the file tree above: `CalendarStore`/
    // `AddressbookStore` already namespace every operation by a `user`
    // argument internally (`<root>/<user>/<calendar-or-addressbook-id>/`,
    // validated the same way a file path is -- see those types' own
    // module docs in breadcloud-storage), so passing the authenticated
    // username through (which `breadcloud-server::combined` now
    // validates against the URL on every request -- see that module's
    // `url_scoped_user`) already gets the same real per-account isolation
    // a separate root per account would, with no need to actually give
    // each account one.
    let calendars = CalendarStore::new(calendars_root.clone());
    let caldav_state = breadcloud_caldav::AppState::new(calendars.clone());

    let addressbooks = AddressbookStore::new(addressbooks_root.clone());
    let carddav_state = breadcloud_carddav::AppState::new(addressbooks.clone());

    if loaded.accounts.is_empty() {
        tracing::warn!(
            "no accounts configured: server is running without authentication as account \"{}\"",
            loaded.default_user
        );
    } else {
        tracing::info!(
            "multi-user auth enabled for {} account(s): {}",
            loaded.accounts.len(),
            usernames.join(", ")
        );
    }

    // Bootstrap the baseline personal/family/school calendar set and a
    // default contacts addressbook for every account (CLOUD-6/CLOUD-7's
    // original bootstrap, now per-account rather than for one fixed
    // placeholder owner -- CLOUD-11 made "owner" a real set of
    // independent accounts instead of a single implicit one). Idempotent:
    // a restart against already-populated roots just no-ops here for
    // every account that already has them.
    for username in &usernames {
        for (id, name, color) in [
            ("personal", "Personal", Some("#1d7dd6")),
            ("family", "Family", Some("#2fa84f")),
            ("school", "School", Some("#c9622a")),
        ] {
            calendars
                .ensure_calendar(username, id, name, color)
                .await
                .unwrap_or_else(|e| {
                    panic!("failed to bootstrap calendar {id} for {username}: {e}")
                });
        }
        addressbooks
            .ensure_addressbook(username, "contacts", "Contacts")
            .await
            .unwrap_or_else(|e| {
                panic!("failed to bootstrap addressbook contacts for {username}: {e}")
            });
    }

    tracing::info!(
        "serving {} account(s) on {} (root: {}, chunked-upload staging: {}, version history: {}, \
         trash: {}, calendars: {}, addressbooks: {})",
        usernames.len(),
        addr,
        root.display(),
        uploads_root.display(),
        versions_root.display(),
        trash_root.display(),
        calendars_root.display(),
        addressbooks_root.display()
    );

    let account_store = AccountStore::new(loaded.accounts);
    let app = combined::router(
        account_store,
        webdav_by_user,
        caldav_state,
        carddav_state,
        loaded.default_user,
    );
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server error");
}
