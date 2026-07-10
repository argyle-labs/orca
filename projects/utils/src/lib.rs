//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http).
//!
//! Filesystem-flavored modules (fs/fs_native/fs_tools/embedded/tree/markdown)
//! moved to the `files` crate (2026-05-29 fs consolidation) — utils only
//! keeps strictly cross-cutting primitives now.

// ── Light always-on core ──────────────────────────────────────────────────
// These modules link only small, self-contained libs (uuid/urlencoding/base64/
// sha2/blake3/chrono/schemars) — no contract/dispatch/tokio/glob. This is the
// slice `plugin_toolkit` re-exports to plugins, so it must stay thin.
/// Atomic file writes (temp + fsync + rename). orca-owned; lives in the leaf so
/// even `utils::state` can use it.
pub mod atomic;
/// Base64 encode/decode (standard + url-safe). orca-owned; the base64 lib is
/// hidden.
pub mod encoding;
pub mod hash;
/// Time-ordered unique ID generation. orca-owned; the UUID lib is hidden.
pub mod id;
pub mod json_schema;
pub mod jsonrpc;
pub mod mesh_status;
pub mod path;
pub mod time;
/// URL percent-encoding + base/path join. orca-owned; the urlencoding lib is
/// hidden.
pub mod url;

// ── Feature-gated modules (heavier deps) ──────────────────────────────────
/// Async framing/shutdown helpers (tokio). Gated by the `rt` feature.
#[cfg(feature = "rt")]
pub mod framing;
/// libgit2 helpers. Gated by the `git` feature (vendored static libgit2).
#[cfg(feature = "git")]
pub mod git;
/// HTTP client + TLS. Gated by the `http` feature (reqwest + rustls stack).
#[cfg(feature = "http")]
pub mod http;
/// X.509 + key generation. Gated by the `pki` feature.
#[cfg(feature = "pki")]
pub mod pki;
/// Cron scheduling. Gated by the `schedule` feature; hides the cron lib (and,
/// with it, chrono) behind a `Timestamp`-only surface.
#[cfg(feature = "schedule")]
pub mod schedule;
/// Glob matching. Gated by the `search` feature.
#[cfg(feature = "search")]
pub mod search;
/// Cooperative shutdown token (tokio). Gated by the `rt` feature.
#[cfg(feature = "rt")]
pub mod shutdown;
/// Daemon state file. Gated by the `state` feature (pulls `contract` + tokio).
#[cfg(feature = "state")]
pub mod state;
/// YAML deserialization. Gated by the `yaml` feature; the YAML lib is hidden.
#[cfg(feature = "yaml")]
pub mod yaml;
