//! Filesystem and search helpers.
//!
//! Modules:
//! - [`fs`] — read/write/edit/exists/mkdir/remove + tilde expansion
//! - [`atomic`] — atomic write (temp + rename)
//! - [`hash`] — sha256 / blake3 file hashing
//! - [`watch`] — async filesystem change notifications
//! - [`search`] — glob + grep helpers

pub mod atomic;
pub mod ops;
pub use ops::*;
pub mod hash;
pub mod search;
pub mod watch;
