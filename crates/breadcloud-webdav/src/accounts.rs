//! Multi-user account model (CLOUD-11): a small, fixed set of
//! independently authenticated, independently quota'd accounts. No roles,
//! no permissions, no sharing between accounts, no self-service account
//! creation -- `breadcloud-server` reads the account list once at startup
//! from environment variables (see that binary's `main.rs`) and never
//! mutates it while running. See the workspace README's "Multi-user
//! accounts" section for the full reasoning and the env var scheme.
//!
//! This lives in `breadcloud-webdav` (not `breadcloud-storage`, which has
//! no HTTP or credential-checking concerns at all, and not
//! `breadcloud-server`, which has no other crate depending on it) because
//! `breadcloud-caldav` and `breadcloud-carddav` already depend on this
//! crate for `check_basic_auth`, the same "shared placeholder auth"
//! relationship this generalizes.

use std::collections::HashMap;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::http::HeaderMap;
use base64::Engine;
use rand_core::OsRng;

/// One account: a username, an argon2id password hash, and a storage
/// quota in bytes.
///
/// The password itself is never retained: [`Account::new`] hashes it
/// immediately and only the hash lives in this struct from then on, the
/// same "don't keep around more than you need" stance the rest of this
/// codebase takes with sensitive content (e.g. a quota-rejected write
/// never touching disk at all -- see `breadcloud_storage::Storage::check_quota`).
/// This does not defend against a compromised *running* server (the
/// plaintext necessarily exists for a moment, in the request and while
/// hashing, same as any password-checking server), only against the
/// hash being the only thing that persists anywhere afterward.
#[derive(Clone)]
pub struct Account {
    pub username: String,
    password_hash: String,
    pub quota_bytes: u64,
}

impl Account {
    /// Hash `password` with argon2id (the password-hashing competition
    /// winner and the current standard recommendation -- deliberately not
    /// a faster general-purpose hash like SHA-256, which is exactly wrong
    /// for passwords: cheap-to-compute is the property you don't want
    /// here) and build an account around the result. `quota_bytes` is the
    /// account's total storage allotment; see
    /// `breadcloud_storage::Storage::check_quota`/`quota_report` for where
    /// it actually gets enforced and reported.
    pub fn new(username: impl Into<String>, password: &str, quota_bytes: u64) -> Self {
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .expect("argon2 hashing a non-empty password should never fail")
            .to_string();
        Account {
            username: username.into(),
            password_hash,
            quota_bytes,
        }
    }

    fn verify(&self, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(&self.password_hash) else {
            // The hash this account was built with is not well-formed
            // argon2 PHC output. This should be unreachable (the only
            // constructor always produces a valid one), but a malformed
            // hash must fail closed, not panic or accept anything.
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }
}

/// The full set of accounts a `breadcloud-server` instance knows about.
/// An empty store means "no accounts configured" -- the open,
/// unauthenticated local-dev mode `breadcloud_webdav::auth` and this
/// crate's callers have always supported, preserved unchanged by
/// CLOUD-11 (see `breadcloud-server`'s `main.rs`).
#[derive(Clone, Default)]
pub struct AccountStore {
    accounts: HashMap<String, Account>,
}

impl AccountStore {
    pub fn new(accounts: Vec<Account>) -> Self {
        AccountStore {
            accounts: accounts
                .into_iter()
                .map(|a| (a.username.clone(), a))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn get(&self, username: &str) -> Option<&Account> {
        self.accounts.get(username)
    }

    pub fn usernames(&self) -> impl Iterator<Item = &str> {
        self.accounts.keys().map(String::as_str)
    }

    /// Check a plain username/password pair against this account set,
    /// returning the authenticated username on success. This is the one
    /// place credential checking actually happens; [`authenticate_request`]
    /// (Basic auth) and `breadcloud-login`'s browser-facing form (CLOUD-28,
    /// login flow v2) both funnel through this instead of each re-deriving
    /// their own check, so there is exactly one credential path for the
    /// whole server regardless of which surface a client authenticates
    /// through (per CLOUD-28's own scoping note: login flow v2 must
    /// integrate with this account model, not stand up a parallel one).
    /// Username and password are checked together (`self.accounts.get(user)`
    /// then verified against *that* account's own hash): this is not "does
    /// this password match any configured account", which would let one
    /// account's credentials authenticate as a different username.
    ///
    /// [`authenticate_request`]: AccountStore::authenticate_request
    pub fn authenticate(&self, username: &str, password: &str) -> Option<String> {
        let account = self.accounts.get(username)?;
        if account.verify(password) {
            Some(account.username.clone())
        } else {
            None
        }
    }

    /// Parse HTTP Basic credentials off `headers` and check them against
    /// this account set, returning the authenticated username on success.
    ///
    /// Deliberately returns the username, not `bool`, unlike
    /// `crate::auth::check_basic_auth` (the single-credential placeholder
    /// this multi-account model replaces for the real server): with more
    /// than one account, a downstream handler needs to know *which*
    /// account authenticated -- that identity is what selects the right
    /// per-user storage roots and is what every per-user isolation check
    /// this issue adds is built on (see `breadcloud-server::combined`).
    pub fn authenticate_request(&self, headers: &HeaderMap) -> Option<String> {
        let value = headers.get("Authorization")?.to_str().ok()?;
        let encoded = value.strip_prefix("Basic ")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (user, pass) = text.split_once(':')?;
        self.authenticate(user, pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_basic_auth(user: &str, pass: &str) -> HeaderMap {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", format!("Basic {encoded}").parse().unwrap());
        headers
    }

    #[test]
    fn authenticates_with_correct_credentials() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let headers = headers_with_basic_auth("alice", "secret1");
        assert_eq!(
            store.authenticate_request(&headers),
            Some("alice".to_string())
        );
    }

    #[test]
    fn rejects_wrong_password() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let headers = headers_with_basic_auth("alice", "wrong");
        assert_eq!(store.authenticate_request(&headers), None);
    }

    #[test]
    fn rejects_unknown_username() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let headers = headers_with_basic_auth("bob", "secret1");
        assert_eq!(store.authenticate_request(&headers), None);
    }

    /// One account's password must not authenticate as a different
    /// account -- catches an implementation that checked "does this
    /// password match *any* configured account" instead of the named
    /// one's own hash.
    #[test]
    fn one_account_s_password_does_not_authenticate_as_another() {
        let store = AccountStore::new(vec![
            Account::new("alice", "secret1", 100),
            Account::new("bob", "secret2", 100),
        ]);
        let headers = headers_with_basic_auth("bob", "secret1");
        assert_eq!(store.authenticate_request(&headers), None);
    }

    #[test]
    fn missing_authorization_header_is_none() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        assert_eq!(store.authenticate_request(&HeaderMap::new()), None);
    }
}
