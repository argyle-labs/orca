//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http).
//!
//! Filesystem-flavored modules (fs/fs_native/fs_tools/embedded/tree/markdown)
//! moved to the `files` crate (2026-05-29 fs consolidation) — utils only
//! keeps strictly cross-cutting primitives now.

pub mod framing;
/// libgit2 helpers. Gated by the `git` feature (vendored static libgit2).
#[cfg(feature = "git")]
pub mod git;
pub mod hash;
/// HTTP client + TLS. Gated by the `http` feature (reqwest + rustls stack).
#[cfg(feature = "http")]
pub mod http;
pub mod json_schema;
pub mod jsonrpc;
pub mod mesh_status;
pub mod path;
/// X.509 + key generation. Gated by the `pki` feature.
#[cfg(feature = "pki")]
pub mod pki;
pub mod search;
pub mod shutdown;
pub mod state;
pub mod time;
