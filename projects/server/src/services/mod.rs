//! Server-side implementations of every per-domain service trait
//! (`auth::{auth,pki,secrets}`, `fleet::lifecycle`,
//! `platform::{db_admin,profile}`, `agents::*`, `plugins::*`, `docs::*`,
//! `docker::service_trait`). MCP registry tools moved to the `mcp` crate (no service trait).
//! `system` was dissolved into the `system` crate (slice A4).
//!
//! Every channel (REST, MCP stdio, CLI, WASM client) dispatches tool calls
//! through the same `orca_dispatch::dispatch` free fn; this module supplies
//! the concrete `Server*` service impls those tools consult via `ToolCtx`.
//! Previously lived in
//! `crate::mcp::*_service` — that path conflated "MCP protocol" with
//! "server-side tool plumbing"; only the former belongs in `mcp/`.

pub mod agent_resolve;
pub mod agents;
pub mod docs;
pub mod lifecycle;
pub mod pod;
pub mod profile;
pub mod spec_registry;
