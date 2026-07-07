//! Shared, broadly-reusable utilities for orca. Each submodule was its
//! own crate prior to consolidation; merging cut binary count and
//! keeps the dep graph shallow. Modules are independent except where
//! noted (graphql uses http).
//!
//! Filesystem-flavored modules (fs/fs_native/fs_tools/embedded/tree/markdown)
//! moved to the `files` crate (2026-05-29 fs consolidation) — utils only
//! keeps strictly cross-cutting primitives now.

pub mod framing;
pub mod git;
pub mod hash;
pub mod http;
pub mod json_schema;
pub mod jsonrpc;
pub mod mesh_status;
pub mod path;
pub mod pki;
pub mod search;
pub mod shutdown;
pub mod state;
pub mod time;
