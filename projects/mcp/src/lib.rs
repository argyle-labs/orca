//! MCP serving core — the long-lived `McpPool` JSON-RPC client used by the
//! stdio `mcp-serve` loop and the HTTP `/api/mcp/*` handlers, the shared wire
//! types, and the Context7 documentation proxy.
//!
//! The federation *tool surface* (`mcp.list` / `mcp.run` / … registrar plus the
//! registered-server sync) lives in the external `argyle-labs/mcp` cdylib and
//! loads at runtime; only the serving primitives consumed at compile time by
//! `server` and `spec` stay in-tree here.

pub mod types;

pub mod client;

pub mod context7;
