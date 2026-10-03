//! Developer tooling for orca — code that only runs from a workspace
//! checkout. Holds:
//!
//! - [`mode`] — `orca dev enable / disable / sync` cargo-watch supervisor.
//! - [`dev_serve`] — HTTP server that streams workspace-built binaries to
//!   peers configured to fetch from a `dev_source` URL.
//! - [`sweep`] — workspace audits (cargo-machete / cargo-deny).
//!
//! Dev tooling MAY call into the `system` and `mesh` crates (or anywhere
//! else it needs to). Those crates do NOT call back into `dev` — that's
//! how we keep production peers from carrying the dev surface.

pub mod dev_serve;
pub mod mode;
pub mod sweep;

/// Process-wide lock for tests that mutate or depend on the environment.
///
/// `std::env` is process-global, so a test that clears `PATH` to build a
/// hermetic fixture blackholes any *other* test thread that happens to be
/// spawning a subprocess at that moment. Crate-level so every module's
/// env-sensitive tests serialize against each other, not just within a file.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires [`ENV_LOCK`], ignoring poisoning — a panicking test must not
/// cascade into unrelated failures in every test that follows it.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
