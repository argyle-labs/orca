//! Low-level filesystem path/io primitives. Generic content/algorithm
//! helpers that *happen* to operate on files (hashing, search) used to
//! live here; they were promoted to top-level `utils::hash` and
//! `utils::search` so the `fs` namespace doesn't shadow the `fs`
//! platform crate.
//!
//! Modules:
//! - [`ops`] (re-exported flat) — read/write/edit/exists/mkdir/remove + tilde expansion + orca_home
//! - [`atomic`] — atomic write (temp + rename)
//! - [`watch`] — async filesystem change notifications

pub mod atomic;
pub mod ops;
pub use ops::*;
pub mod watch;
