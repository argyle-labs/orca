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
/// Unified per-peer reachability source of truth (class + backoff/dormant state
/// machine) shared by the pod liveness refresher, `db` replicate.pull, and
/// roster-sync so all three agree on which peers to dial. Std-only.
pub mod reachability;
/// Ordered endpoint reachability paths — the ONE shared `Route` type used by
/// mesh (contract/db/pod) and plugins alike. No scalar URL/host fields anywhere.
pub mod route;
pub mod time;
/// URL percent-encoding + base/path join. orca-owned; the urlencoding lib is
/// hidden.
pub mod url;

/// Projects when a filling resource runs out, from usage samples. Pure maths —
/// a static "warn at 85%" both cries wolf on a parked filesystem and stays
/// silent on one with four days left; this reports the derivative instead.
pub mod capacity_trend;

// ── Feature-gated modules (heavier deps) ──────────────────────────────────
/// Syntax validation for managed config files (json/yaml/toml/xml). Gated by the
/// `config_format` feature, which pulls the toml + yaml + xml parsers. Exists so
/// orca never leaves a service on a config file it cannot parse.
#[cfg(feature = "config_format")]
pub mod config_format;
/// Detects config-parse failures a service already logged — the read-side half of
/// [`config_format`]'s write-side guard, for the files orca did not write. Shares
/// the `config_format` feature because it reports the same `ConfigFormat`.
#[cfg(feature = "config_format")]
pub mod config_parse_log;
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
