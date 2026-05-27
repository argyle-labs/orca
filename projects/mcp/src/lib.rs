//! MCP domain — registered-MCP-server registry + tool mappings, federation
//! passthrough (`list_mcp_tools` / `run_mcp_tool`), and the long-lived
//! `McpPool` JSON-RPC client used by both the orca_tools and the HTTP
//! `/api/mcp/*` handlers.

pub mod types;

#[cfg(feature = "native")]
pub mod client;

#[cfg(feature = "native")]
pub mod sync;

#[cfg(feature = "native")]
pub mod tools;

#[cfg(feature = "native")]
pub mod context7;

#[cfg(feature = "native")]
pub mod specs;

#[cfg(feature = "native")]
pub mod spec_tools;
