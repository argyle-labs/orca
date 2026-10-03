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

/// Held by every test that mutates `std::env` or spawns a subprocess. Crate-wide:
/// `std::env` is process-global, so a per-module lock leaves the spawners racing.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Ignores poisoning: one panicking test must not fail every later test.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
