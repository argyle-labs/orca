// This is an HTTP integration harness: request bodies and decoded responses are
// genuinely free-form JSON spanning every endpoint under test, so `serde_json::Value`
// is the right type here rather than a per-endpoint typed struct.
#![allow(clippy::disallowed_types)]

//! Shared harness for the daemon HTTP integration tier.
//!
//! Drives the REAL axum router (`orca::serve::build_router`) in-process via
//! `tower::ServiceExt::oneshot` — no bound port, no TLS, no subprocess, no
//! signals. Every test runs under a process-wide env lock while ORCA_HOME /
//! HOME / ORCA_DB_PATH point at a fresh tempdir, so parallel nextest workers
//! never stomp each other's sqlite file (mirrors `auth/tests/login_e2e.rs`).

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tower::ServiceExt;

/// Global serialization for tests that mutate process-wide env vars
/// (ORCA_HOME, HOME, ORCA_DB_PATH). Without this, parallel tests race and
/// stomp each other's tempdirs → "database is locked" / "disk I/O error".
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// An isolated environment: holds the env lock and a fresh tempdir for the
/// whole test. Drop releases the lock (and removes the tempdir).
pub struct IsolatedEnv {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    pub db_path: PathBuf,
}

impl IsolatedEnv {
    /// Build the real router against this env's isolated DB. `dev=true` mounts
    /// the dev fallback (proxy) but that path is never exercised by these tests
    /// — they only hit concrete registered routes.
    pub fn router(&self) -> Router {
        orca::serve::build_router(true, self.db_path.clone())
    }
}

/// Set up an isolated env: hold the env lock for the whole test and pin
/// ORCA_HOME / HOME / ORCA_DB_PATH to a fresh tempdir. The returned guard must
/// be kept alive for the duration of the test.
pub fn with_isolated_env() -> IsolatedEnv {
    let guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("orca.db");
    // SAFETY: env mutation is serialized by `env_lock()` above and held for the
    // whole test via the returned guard.
    unsafe {
        std::env::set_var("ORCA_HOME", dir.path());
        std::env::set_var("HOME", dir.path());
        std::env::set_var("ORCA_DB_PATH", &db_path);
    }
    IsolatedEnv {
        _guard: guard,
        _dir: dir,
        db_path,
    }
}

/// Mint a real bearer token of the given role directly in the isolated DB and
/// return its plaintext. The stored row is `sha256(plaintext)` exactly as the
/// `auth.token.create` tool would write — so the bearer auth path
/// (`try_token_auth` → `find_by_hash`) is genuinely exercised.
pub fn mint_token(env: &IsolatedEnv, role: &str) -> String {
    // `open_default` reads ORCA_DB_PATH from the env this isolated fixture set.
    // Guard against a mis-ordered call (minting before `with_isolated_env`): the
    // tempdir holding the DB must still be alive.
    assert!(
        env.db_path.parent().is_some_and(std::path::Path::exists),
        "isolated env tempdir must exist before minting a token"
    );
    // Ensures the DB + schema exist (open_default runs migrations).
    let conn = db::open_default().unwrap();
    let mut raw = [0u8; 16];
    // Deterministic-enough uniqueness without pulling `rand`: derive from a
    // fresh uuid so parallel mints never collide.
    let unique = utils::id::new();
    let seed = unique.as_bytes();
    for (i, b) in raw.iter_mut().enumerate() {
        *b = seed.get(i).copied().unwrap_or(0);
    }
    let plaintext = format!("orca_{}", hex_lower(&raw));
    let hash = utils::hash::sha256_hex(plaintext.as_bytes());
    let id = utils::id::new();
    let now = utils::time::now_rfc3339();
    auth::api_tokens::insert(
        &conn,
        &id,
        "integration",
        &hash,
        role,
        &now,
        None,
        None,
        false,
    )
    .unwrap();
    plaintext
}

/// Admin-role token convenience wrapper.
pub fn mint_admin_token(env: &IsolatedEnv) -> String {
    mint_token(env, "admin")
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String never fails");
    }
    s
}

/// Drive one request through the router and decode the JSON body.
///
/// A loopback `ConnectInfo` is injected on every request because several
/// handlers (`bootstrap_status_handler`) and the auth middleware extract it;
/// `oneshot` does not populate it the way `into_make_service_with_connect_info`
/// would, so we insert it manually.
pub async fn oneshot_json(
    router: Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = oneshot_raw(router, method, path, bearer, body).await;
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Like [`oneshot_json`] but returns the raw body bytes (for non-JSON routes
/// such as `/scalar`, and for asserting on error text).
pub async fn oneshot_raw(
    router: Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(tok) = bearer {
        builder = builder.header(axum::http::header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    let req_body = match &body {
        Some(json) => {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(json).unwrap())
        }
        None => Body::empty(),
    };
    let mut req = builder.body(req_body).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));

    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (status, bytes)
}
