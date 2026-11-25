//! Multi-user account model (CLOUD-11): a small, fixed set of
//! independently authenticated, independently quota'd accounts. No roles,
//! no permissions, no sharing between accounts, no self-service account
//! creation -- `breadcloud-server` reads the account list once at startup
//! from environment variables (see that binary's `main.rs`) and never
//! mutates it while running. See docs/DESIGN.md's "Multi-user
//! accounts" section for the full reasoning and the env var scheme.
//!
//! This lives in `breadcloud-webdav` (not `breadcloud-storage`, which has
//! no HTTP or credential-checking concerns at all, and not
//! `breadcloud-server`, which has no other crate depending on it) because
//! `breadcloud-caldav` and `breadcloud-carddav` already depend on this
//! crate for `check_basic_auth`, the same "shared placeholder auth"
//! relationship this generalizes.

use std::collections::{hash_map::RandomState, HashMap, VecDeque};
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
#[derive(Clone)]
pub struct AccountStore {
    accounts: HashMap<String, Account>,
    // Unknown usernames use this account's hash so a miss takes the same
    // Argon2 path as a known username with an incorrect password.
    dummy_account: Account,
    protection: Arc<Mutex<AuthProtection>>,
}

impl Default for AccountStore {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

// A desktop sync can send many requests over this brief window; keep the
// cache short-lived and bounded so changed credentials take effect quickly.
const AUTH_CACHE_TTL: Duration = Duration::from_secs(15);
const AUTH_CACHE_CAPACITY: usize = 4096;
// Permit five failures in a rolling minute per peer and per
// username, then reject attempts until the oldest failure leaves the window.
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const AUTH_FAILURE_LIMIT: usize = 5;
const AUTH_FAILURE_BUCKET_CAPACITY: usize = 8192;
type PeerUser = (IpAddr, String);

#[derive(Default)]
struct AuthProtection {
    cache_hasher: RandomState,
    successful: HashMap<u64, (String, Instant)>,
    failures_by_peer_user: HashMap<PeerUser, VecDeque<Instant>>,
    failures_by_user: HashMap<String, VecDeque<Instant>>,
    in_flight_by_peer_user: HashMap<PeerUser, usize>,
    in_flight_by_user: HashMap<String, usize>,
    in_flight: usize,
}

impl AuthProtection {
    fn cache_key(&self, header: &[u8]) -> u64 {
        self.cache_hasher.hash_one(header)
    }

    fn cached_user(&mut self, header: &[u8], now: Instant) -> Option<String> {
        let key = self.cache_key(header);
        match self.successful.get(&key) {
            Some((user, expires)) if *expires > now => Some(user.clone()),
            Some(_) => {
                self.successful.remove(&key);
                None
            }
            None => None,
        }
    }

    fn cache_success(&mut self, header: &[u8], username: &str, now: Instant) {
        if self.successful.len() >= AUTH_CACHE_CAPACITY {
            self.successful.retain(|_, (_, expires)| *expires > now);
        }
        if self.successful.len() >= AUTH_CACHE_CAPACITY {
            if let Some(key) = self.successful.keys().next().copied() {
                self.successful.remove(&key);
            }
        }
        self.successful.insert(
            self.cache_key(header),
            (username.to_string(), now + AUTH_CACHE_TTL),
        );
    }

    fn check_allowed(&mut self, ip: IpAddr, user: &str, now: Instant) -> Result<(), Duration> {
        let peer_user = (ip, user.to_owned());
        prune_failure_bucket(&mut self.failures_by_peer_user, &peer_user, now);
        prune_failure_bucket(&mut self.failures_by_user, &user.to_string(), now);
        let peer_user_retry = blocked_for(&self.failures_by_peer_user, &peer_user, now);
        let user_retry = blocked_for(&self.failures_by_user, &user.to_string(), now);
        match (peer_user_retry, user_retry) {
            (Some(peer_user), Some(user)) => Err(peer_user.max(user)),
            (Some(retry), None) | (None, Some(retry)) => Err(retry),
            (None, None) => Ok(()),
        }
    }

    // Reserve admission while holding the same mutex as the failure
    // budget. Concurrent requests must count before Argon2 starts.
    fn reserve_verification(
        &mut self,
        ip: IpAddr,
        user: &str,
        now: Instant,
    ) -> Result<(), Duration> {
        self.check_allowed(ip, user, now)?;
        let peer_user = (ip, user.to_owned());
        let peer_user_count = self
            .failures_by_peer_user
            .get(&peer_user)
            .map_or(0, VecDeque::len)
            + self
                .in_flight_by_peer_user
                .get(&peer_user)
                .copied()
                .unwrap_or(0);
        let user_count = self.failures_by_user.get(user).map_or(0, VecDeque::len)
            + self.in_flight_by_user.get(user).copied().unwrap_or(0);
        if peer_user_count >= AUTH_FAILURE_LIMIT
            || user_count >= AUTH_FAILURE_LIMIT
            || self.in_flight >= AUTH_FAILURE_BUCKET_CAPACITY
        {
            return Err(AUTH_FAILURE_WINDOW);
        }
        *self.in_flight_by_peer_user.entry(peer_user).or_default() += 1;
        *self.in_flight_by_user.entry(user.to_owned()).or_default() += 1;
        self.in_flight += 1;
        Ok(())
    }

    fn finish_verification(&mut self, ip: IpAddr, user: &str, success: bool, now: Instant) {
        let peer_user = (ip, user.to_owned());
        if let Some(count) = self.in_flight_by_peer_user.get_mut(&peer_user) {
            *count -= 1;
            if *count == 0 {
                self.in_flight_by_peer_user.remove(&peer_user);
            }
        }
        if let Some(count) = self.in_flight_by_user.get_mut(user) {
            *count -= 1;
            if *count == 0 {
                self.in_flight_by_user.remove(user);
            }
        }
        self.in_flight -= 1;
        if !success {
            self.record_failure(ip, user, now);
        }
    }

    fn record_failure(&mut self, ip: IpAddr, user: &str, now: Instant) {
        add_failure(&mut self.failures_by_peer_user, (ip, user.to_owned()), now);
        add_failure(&mut self.failures_by_user, user.to_string(), now);
    }
}

fn prune_failure_bucket<K: Eq + std::hash::Hash>(
    buckets: &mut HashMap<K, VecDeque<Instant>>,
    key: &K,
    now: Instant,
) {
    let empty = buckets.get_mut(key).is_some_and(|attempts| {
        while attempts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) >= AUTH_FAILURE_WINDOW)
        {
            attempts.pop_front();
        }
        attempts.is_empty()
    });
    if empty {
        buckets.remove(key);
    }
}

fn blocked_for<K: Eq + std::hash::Hash>(
    buckets: &HashMap<K, VecDeque<Instant>>,
    key: &K,
    now: Instant,
) -> Option<Duration> {
    buckets.get(key).and_then(|attempts| {
        (attempts.len() >= AUTH_FAILURE_LIMIT).then(|| {
            attempts
                .front()
                .map(|first| AUTH_FAILURE_WINDOW.saturating_sub(now.duration_since(*first)))
                .unwrap_or_default()
        })
    })
}

fn add_failure<K: Clone + Eq + std::hash::Hash>(
    buckets: &mut HashMap<K, VecDeque<Instant>>,
    key: K,
    now: Instant,
) {
    prune_failure_bucket(buckets, &key, now);
    if buckets.len() >= AUTH_FAILURE_BUCKET_CAPACITY && !buckets.contains_key(&key) {
        if let Some(oldest) = buckets.keys().next().cloned() {
            // The rolling-window pruning above normally frees entries. If
            // an attacker supplies many distinct usernames within one
            // window, evict one bucket to keep this process-local guard
            // bounded rather than allowing unbounded memory growth.
            buckets.remove(&oldest);
        }
    }
    let attempts = buckets.entry(key).or_default();
    attempts.push_back(now);
}

impl AccountStore {
    pub fn new(accounts: Vec<Account>) -> Self {
        AccountStore {
            accounts: accounts
                .into_iter()
                .map(|a| (a.username.clone(), a))
                .collect(),
            dummy_account: Account::new("", "breadcloud-unknown-account-placeholder", 0),
            protection: Arc::default(),
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
    /// returning the authenticated username on success. `breadcloud-login`'s
    /// browser-facing form (CLOUD-28, login flow v2) uses this instead of
    /// re-deriving its own check, so every authentication surface checks
    /// credentials against the same account hashes, whether it arrives
    /// through Basic auth ([`authenticate_request`]) or the login form
    /// (per CLOUD-28's own scoping note: login flow v2 must integrate with
    /// this account model, not stand up a parallel one).
    /// Username and password are checked together (`self.accounts.get(user)`
    /// then verified against *that* account's own hash): this is not "does
    /// this password match any configured account", which would let one
    /// account's credentials authenticate as a different username. An
    /// unknown username still runs the password verifier against the dummy
    /// account, the same equalized-timing defense [`authenticate_request`]
    /// applies, so a login attempt's duration does not reveal whether the
    /// account exists.
    ///
    /// This entry point is deliberately not rate-limited: the login-flow
    /// form carries its own concurrency bound and expiry, while the Basic
    /// auth path applies [`AccountStore::authenticate_request_from`]'s
    /// per-peer budget, which needs the request's peer address.
    ///
    /// [`authenticate_request`]: AccountStore::authenticate_request
    pub fn authenticate(&self, username: &str, password: &str) -> Option<String> {
        match self.accounts.get(username) {
            Some(account) => {
                if account.verify(password) {
                    Some(account.username.clone())
                } else {
                    None
                }
            }
            None => {
                let _ = self.dummy_account.verify(password);
                None
            }
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
        self.authenticate_request_from(headers, IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .ok()
            .flatten()
    }

    /// Authenticate a request while tracking failures against the peer and
    /// supplied username together, and against the username across peers.
    /// A reverse proxy can share one peer address across unrelated users, so
    /// the peer budget is account-aware. Forwarded headers are not trusted.
    /// The Basic auth header is never retained: successful checks are cached
    /// for 15 seconds under a process-keyed fast hash, with a fixed capacity.
    pub fn authenticate_request_from(
        &self,
        headers: &HeaderMap,
        peer_ip: IpAddr,
    ) -> Result<Option<String>, Duration> {
        self.authenticate_with_verifier(headers, peer_ip, Account::verify)
    }

    fn authenticate_with_verifier(
        &self,
        headers: &HeaderMap,
        peer_ip: IpAddr,
        verify: impl FnOnce(&Account, &str) -> bool,
    ) -> Result<Option<String>, Duration> {
        let Some(value) = headers.get("Authorization") else {
            let mut protection = self.protection.lock().expect("auth state poisoned");
            let now = Instant::now();
            protection.check_allowed(peer_ip, "", now)?;
            protection.record_failure(peer_ip, "", now);
            return Ok(None);
        };
        let Ok(value) = value.to_str() else {
            let mut protection = self.protection.lock().expect("auth state poisoned");
            let now = Instant::now();
            protection.check_allowed(peer_ip, "", now)?;
            protection.record_failure(peer_ip, "", now);
            return Ok(None);
        };
        let mut protection = self.protection.lock().expect("auth state poisoned");
        let now = Instant::now();
        if let Some(username) = protection.cached_user(value.as_bytes(), now) {
            return Ok(Some(username));
        }

        // Parse enough to choose the peer/user and per-user buckets before
        // the expensive Argon2 verify. Malformed credentials use an empty
        // username in the peer bucket.
        let parsed = value
            .strip_prefix("Basic ")
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
            })
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .and_then(|text| {
                text.split_once(':')
                    .map(|(user, pass)| (user.to_string(), pass.to_string()))
            });
        let username = parsed.as_ref().map_or("", |(user, _)| user.as_str());
        protection.reserve_verification(peer_ip, username, now)?;
        drop(protection);

        let Some((user, pass)) = parsed else {
            self.protection
                .lock()
                .expect("auth state poisoned")
                .finish_verification(peer_ip, "", false, Instant::now());
            return Ok(None);
        };
        let Some(account) = self.accounts.get(&user) else {
            let _ = verify(&self.dummy_account, &pass);
            self.protection
                .lock()
                .expect("auth state poisoned")
                .finish_verification(peer_ip, &user, false, Instant::now());
            return Ok(None);
        };
        if verify(account, &pass) {
            let mut protection = self.protection.lock().expect("auth state poisoned");
            protection.finish_verification(peer_ip, &user, true, Instant::now());
            protection.cache_success(value.as_bytes(), &account.username, Instant::now());
            Ok(Some(account.username.clone()))
        } else {
            self.protection
                .lock()
                .expect("auth state poisoned")
                .finish_verification(peer_ip, &user, false, Instant::now());
            Ok(None)
        }
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

    #[test]
    fn unknown_username_runs_the_password_verifier() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let headers = headers_with_basic_auth("bob", "wrong");
        let mut verified_dummy = false;
        let result = store.authenticate_with_verifier(
            &headers,
            "192.0.2.9".parse().unwrap(),
            |account, _| {
                assert!(account.username.is_empty());
                verified_dummy = true;
                false
            },
        );

        assert_eq!(result, Ok(None));
        assert!(verified_dummy);
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

    #[test]
    fn failed_authentication_is_limited_by_peer_user_and_username() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let headers = headers_with_basic_auth("alice", "wrong");
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..AUTH_FAILURE_LIMIT {
            assert_eq!(store.authenticate_request_from(&headers, ip), Ok(None));
        }
        assert!(store.authenticate_request_from(&headers, ip).is_err());

        // The username bucket applies across source addresses.
        let other_ip: IpAddr = "192.0.2.2".parse().unwrap();
        assert!(store.authenticate_request_from(&headers, other_ip).is_err());
    }

    #[test]
    fn users_sharing_a_proxy_peer_have_independent_limits_without_trusting_forwarded_headers() {
        let store = AccountStore::new(vec![
            Account::new("alice", "alice-secret", 100),
            Account::new("bob", "bob-secret", 100),
        ]);
        let proxy_peer: IpAddr = "192.0.2.80".parse().unwrap();

        for attempt in 0..AUTH_FAILURE_LIMIT {
            let mut headers = headers_with_basic_auth("alice", "wrong");
            headers.insert(
                "X-Forwarded-For",
                format!("198.51.100.{}", attempt + 1).parse().unwrap(),
            );
            assert_eq!(
                store.authenticate_with_verifier(&headers, proxy_peer, |account, _| {
                    assert_eq!(account.username, "alice");
                    false
                }),
                Ok(None)
            );
        }

        let mut blocked_alice = headers_with_basic_auth("alice", "wrong");
        blocked_alice.insert("X-Forwarded-For", "203.0.113.10".parse().unwrap());
        assert!(store
            .authenticate_with_verifier(&blocked_alice, proxy_peer, |_, _| {
                panic!("throttled request must not enter password verification")
            })
            .is_err());

        let bob = headers_with_basic_auth("bob", "wrong");
        for _ in 0..AUTH_FAILURE_LIMIT {
            assert_eq!(
                store.authenticate_with_verifier(&bob, proxy_peer, |account, _| {
                    assert_eq!(account.username, "bob");
                    false
                }),
                Ok(None)
            );
        }
        assert!(store
            .authenticate_with_verifier(&bob, proxy_peer, |_, _| {
                panic!("throttled request must not enter password verification")
            })
            .is_err());
    }

    #[test]
    fn unknown_user_is_dummy_verified_then_throttled_per_peer_and_name() {
        let store = AccountStore::new(vec![Account::new("alice", "secret1", 100)]);
        let proxy_peer: IpAddr = "192.0.2.80".parse().unwrap();
        let headers = headers_with_basic_auth("unknown", "wrong");
        let mut verified = 0;

        for _ in 0..AUTH_FAILURE_LIMIT {
            assert_eq!(
                store.authenticate_with_verifier(&headers, proxy_peer, |account, _| {
                    assert!(account.username.is_empty());
                    verified += 1;
                    false
                }),
                Ok(None)
            );
        }
        assert!(store
            .authenticate_with_verifier(&headers, proxy_peer, |_, _| {
                panic!("throttled unknown user must not enter password verification")
            })
            .is_err());
        assert_eq!(verified, AUTH_FAILURE_LIMIT);
    }

    #[test]
    fn successful_credential_cache_expires_without_retaining_header_text() {
        let mut protection = AuthProtection::default();
        let header = b"Basic YWxpY2U6c2VjcmV0MQ==";
        let now = Instant::now();
        protection.cache_success(header, "alice", now);
        assert_eq!(protection.cached_user(header, now), Some("alice".into()));
        assert_eq!(protection.successful.len(), 1);
        assert_eq!(protection.successful.values().next().unwrap().0, "alice");
        assert_eq!(protection.cached_user(header, now + AUTH_CACHE_TTL), None);
    }
    #[test]
    fn concurrent_auth_attempts_reserve_the_budget_before_verification() {
        use std::sync::{mpsc, Barrier};
        use std::thread;
        let store = AccountStore::new(vec![Account::new("alice", "secret", 100)]);
        let barrier = Arc::new(Barrier::new(AUTH_FAILURE_LIMIT + 1));
        let (started, admitted) = mpsc::channel();
        let mut threads = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            let barrier = barrier.clone();
            let started = started.clone();
            threads.push(thread::spawn(move || {
                let headers = headers_with_basic_auth("alice", "wrong");
                store.authenticate_with_verifier(&headers, "192.0.2.1".parse().unwrap(), |_, _| {
                    started.send(()).unwrap();
                    barrier.wait();
                    false
                })
            }));
        }
        for _ in 0..AUTH_FAILURE_LIMIT {
            admitted.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        {
            let protection = store.protection.lock().unwrap();
            assert_eq!(protection.in_flight, AUTH_FAILURE_LIMIT);
            assert_eq!(protection.in_flight_by_user["alice"], AUTH_FAILURE_LIMIT);
            assert_eq!(
                protection.in_flight_by_peer_user[&("192.0.2.1".parse().unwrap(), "alice".into())],
                AUTH_FAILURE_LIMIT
            );
        }
        barrier.wait();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| **r == Ok(None)).count(),
            AUTH_FAILURE_LIMIT
        );
        assert_eq!(
            results.iter().filter(|r| r.is_err()).count(),
            16 - AUTH_FAILURE_LIMIT
        );
        let protection = store.protection.lock().unwrap();
        assert_eq!(protection.in_flight, 0);
        assert!(protection.in_flight_by_peer_user.is_empty());
        assert!(protection.in_flight_by_user.is_empty());
        assert_eq!(
            protection.failures_by_user["alice"].len(),
            AUTH_FAILURE_LIMIT
        );
    }

    #[test]
    fn inflight_budgets_apply_across_ips_and_users_and_success_releases_admission() {
        let mut protection = AuthProtection::default();
        let ip = "192.0.2.1".parse().unwrap();
        let other = "192.0.2.2".parse().unwrap();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT {
            protection.reserve_verification(ip, "alice", now).unwrap();
        }
        assert!(protection
            .reserve_verification(other, "alice", now)
            .is_err());
        protection.reserve_verification(ip, "bob", now).unwrap();
        protection.finish_verification(ip, "bob", true, now);
        protection.finish_verification(ip, "alice", true, now);
        protection
            .reserve_verification(other, "alice", now)
            .unwrap();
        protection.finish_verification(other, "alice", false, now);
        for _ in 1..AUTH_FAILURE_LIMIT {
            protection.finish_verification(ip, "alice", false, now);
        }
        assert_eq!(protection.in_flight, 0);
        assert_eq!(
            protection.failures_by_user["alice"].len(),
            AUTH_FAILURE_LIMIT
        );
        assert!(protection
            .reserve_verification(other, "alice", now)
            .is_err());
        assert!(protection
            .reserve_verification(other, "alice", now + AUTH_FAILURE_WINDOW)
            .is_ok());
    }
}
