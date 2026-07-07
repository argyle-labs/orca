//! Process-wide shared SQLCipher connection.
//!
//! Replaces the legacy per-call `db::open_default()` pattern for server-side
//! callers. Each `open_default()` invocation pays ~150ms of PBKDF2 work, the
//! schema-check probe, and allocates an 8 MB rusqlite page cache — at our
//! polling rates (multiple writers + readers at 2s cadence) that compounds
//! into the bulk of daemon RSS and CPU.
//!
//! A single `Arc<Mutex<Connection>>` instead:
//!   * Pays the open/KDF cost ONCE at startup.
//!   * Keeps the rusqlite statement cache hot across calls.
//!   * Returns to a single 8 MB page cache instead of N transient ones.
//!
//! SQLite serializes writes anyway (single-writer), so a Mutex is the right
//! shape. If read contention shows up in profiling we promote to a
//! reader-pool, but in practice individual queries complete in microseconds
//! and the contention bar is high.
//!
//! Test paths and CLI invocations keep using `open_default()` directly —
//! they don't outlive a single call and the task-local override needs to
//! continue working. Only the server's long-running tasks hit the pool.

use crate::open_default;
use anyhow::Result;
use rusqlite::Connection;
use std::sync::{Arc, Mutex, OnceLock};

static POOL: OnceLock<DbPool> = OnceLock::new();

/// Shared, process-wide DB handle. Cheap to clone (single `Arc`).
#[derive(Clone)]
pub struct DbPool {
    conn: Arc<Mutex<Connection>>,
}

impl DbPool {
    /// Open the shared connection. Must be called once at server startup.
    ///
    /// Idempotent: subsequent calls return the existing pool unchanged so
    /// every code path can defensively call `init_or_get` without ordering
    /// fragility.
    pub fn init_or_get() -> Result<Self> {
        if let Some(p) = POOL.get() {
            return Ok(p.clone());
        }
        let conn = open_default()?;
        let pool = DbPool {
            conn: Arc::new(Mutex::new(conn)),
        };
        // Race-safe: if another thread won the init race, drop ours and use
        // theirs. The discarded connection is closed cleanly on drop.
        match POOL.set(pool.clone()) {
            Ok(()) => Ok(pool),
            Err(_) => Ok(POOL.get().expect("just-set").clone()),
        }
    }

    /// Get the already-initialized pool. Returns `None` if `init_or_get`
    /// hasn't been called yet — callers that may run before server startup
    /// (CLI, tests) should fall through to `open_default()`.
    pub fn get() -> Option<Self> {
        POOL.get().cloned()
    }

    /// Run `f` with exclusive access to the shared connection. Held only
    /// for the duration of the closure — keep it short.
    ///
    /// `f` runs on the caller's thread; this is NOT an async boundary.
    /// Wrap in `tokio::task::spawn_blocking` when calling from async code
    /// if the query could be long.
    pub fn with_conn<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R>,
    {
        let guard = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("db pool poisoned: {e}"))?;
        f(&guard)
    }
}

/// Convenience: get the pool if initialized, else fall back to a fresh
/// `open_default()`. Use this from code paths that may run in either
/// server context (pool wins) or CLI/test context (fresh open).
///
/// The fallback path opens a fresh connection per call — fine for CLI
/// (one call per process) and tests (task-local override). Servers MUST
/// have called `DbPool::init_or_get()` at startup or they pay the legacy
/// per-call cost.
pub fn with_pooled_or_open<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R>,
{
    if let Some(pool) = DbPool::get() {
        return pool.with_conn(f);
    }
    let conn = open_default()?;
    f(&conn)
}
