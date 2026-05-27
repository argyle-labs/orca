#![recursion_limit = "256"]
//! Orca server binary — the interactive AI agent orchestrator.
//!
//! Remaining modules pending dissolution: `mcp` (stdio server) and `serve`
//! (Axum HTTP). The rest of the original server crate has been distributed
//! into domain crates.

pub mod mcp;
pub mod serve;
pub mod spec_detail;
