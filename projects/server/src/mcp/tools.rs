//! Context7 federation tool defs. `run_agent` was moved to the
//! `agent.run` `#[orca_tool]` in `conversation/src/run.rs` — picked up by
//! `dispatch::mcp_definitions()`. The two entries below proxy to a
//! remote Context7 MCP server and have no native counterpart in orca's tool
//! inventory.

#![allow(clippy::disallowed_types)] // MCP tool-schema JSON blob — dynamic JSON construction required
use serde_json::{Value, json};

pub fn tool_defs() -> Value {
    json!([
        {
            "name": "resolve_library",
            "description": "Resolve a library name to its Context7-compatible ID. Call before get_library_docs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "libraryName": { "type": "string" }
                },
                "required": ["libraryName"]
            }
        },
        {
            "name": "get_library_docs",
            "description": "Fetch up-to-date documentation for a library via Context7.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "context7CompatibleLibraryID": { "type": "string" },
                    "topic": { "type": "string" },
                    "tokens": { "type": "integer" }
                },
                "required": ["context7CompatibleLibraryID"]
            }
        }
    ])
}
