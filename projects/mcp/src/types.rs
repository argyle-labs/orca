//! Wire types for the `system.mcp.*` and `system.mcp.federation.*` tools.
//!
//! `serde_json::Value` appears here only inside [`mcp_fed`] for MCP
//! protocol-level opaque blobs (input_schema, args, resource,
//! structured_content) whose shapes are defined by upstream MCP servers,
//! not by orca.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Registry CRUD types ─────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MappingEntry {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsServerEntry {
    pub server: String,
    pub added: u32,
    pub skipped: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersOutput {
    pub servers: Vec<McpServerEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AddMcpServerArgs {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(skip)]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerMutationResult {
    pub name: String,
    pub changed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RemoveMcpServerArgs {
    pub name: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct MapToolArgs {
    pub name: String,
    pub orca_tool: String,
    pub external_tool: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MapToolResult {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolArgs {
    pub orca_tool: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolResult {
    pub orca_tool: String,
    pub changed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsOutput {
    pub results: Vec<SyncToolsServerEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsArgs {
    /// Filter by server name (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsOutput {
    pub mappings: Vec<MappingEntry>,
}

// ── MCP federation types (opaque MCP protocol blobs) ────────────────────────

mod mcp_fed {
    use super::*;
    use ::utils::json_schema::JsonSchemaNode;
    use serde_json::Value;

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct McpToolEntry {
        pub server: String,
        pub name: String,
        pub description: String,
        pub input_schema: JsonSchemaNode,
    }

    #[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsArgs {}

    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsOutput {
        pub tools: Vec<McpToolEntry>,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct RunMcpToolArgs {
        pub server: String,
        pub tool: String,
        #[serde(default)]
        pub args: Option<serde_json::Map<String, Value>>,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct McpContent {
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub resource: Option<Value>,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct RunMcpToolOutput {
        pub content: Vec<McpContent>,
        pub is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub structured_content: Option<Value>,
    }
}

pub use mcp_fed::{
    ListMcpToolsArgs, ListMcpToolsOutput, McpContent, McpToolEntry, RunMcpToolArgs,
    RunMcpToolOutput,
};
