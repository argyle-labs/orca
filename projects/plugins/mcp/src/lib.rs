//! MCP domain — registered-MCP-server registry + tool mappings, federation
//! passthrough (`list_mcp_tools` / `run_mcp_tool`), and the long-lived
//! `McpPool` JSON-RPC client used by both the orca_tools and the HTTP
//! `/api/mcp/*` handlers.

pub mod types;

pub mod client;

pub mod sync;

pub mod tools;

pub mod context7;
