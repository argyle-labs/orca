//! Management domain tools — MCP server registry + tool mappings, schema
//! databases, Docker runtimes, doc roots + ignore patterns, Proxmox + Home
//! Assistant endpoints. Run impls dispatch through the six sub-services in
//! `services::mgmt`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::orca_tool;
// Value is used only in MCP-federation inner modules where all Value uses are
// legitimate opaque blobs (MCP protocol-level). The allow on each mod block
// covers derive expansions; this import-level allow covers the import itself.
#[allow(clippy::disallowed_types)]
use serde_json::Value;

// ═══════════════════════════════════════════════════════════════════════════
// MCP servers + tool mappings — shared row shapes
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsServerEntry {
    pub server: String,
    pub added: u32,
    pub skipped: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── list_mcp_servers ────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersOutput {
    pub servers: Vec<McpServerEntry>,
}

// ── add_mcp_server ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddMcpServerArgs {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(skip))]
    pub env: Option<HashMap<String, String>>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerMutationResult {
    pub name: String,
    pub changed: bool,
}

// ── remove_mcp_server ───────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveMcpServerArgs {
    pub name: String,
}

// ── map_tool / unmap_tool ───────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MapToolArgs {
    pub name: String,
    pub orca_tool: String,
    pub external_tool: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MapToolResult {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolArgs {
    pub orca_tool: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolResult {
    pub orca_tool: String,
    pub changed: bool,
}

// ── sync_tools ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsOutput {
    pub results: Vec<SyncToolsServerEntry>,
}

// ── list_tool_mappings ──────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsArgs {
    /// Filter by server name (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsOutput {
    pub mappings: Vec<MappingEntry>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema databases
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaDbEntry {
    pub name: String,
    pub driver: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub user: String,
    pub database: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domains_file: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasOutput {
    pub schemas: Vec<SchemaDbEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddSchemaArgs {
    pub name: String,
    pub database: String,
    pub user: String,
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domains_file: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveSchemaArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Docker runtimes
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DockerRuntimeEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDockerRuntimesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDockerRuntimesOutput {
    pub runtimes: Vec<DockerRuntimeEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddDockerRuntimeArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockerRuntimeMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDockerRuntimeArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Doc roots + ignore patterns
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootRegEntry {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsOutput {
    pub roots: Vec<DocRootRegEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddDocRootArgs {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDocRootArgs {
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsOutput {
    pub patterns: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternArgs {
    pub pattern: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternMutationResult {
    pub pattern: String,
    pub changed: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// Proxmox endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub insecure: bool,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsOutput {
    pub endpoints: Vec<ProxmoxEndpointEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddProxmoxEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub token_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveProxmoxEndpointArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Home Assistant endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HaEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsOutput {
    pub endpoints: Vec<HaEndpointEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddHomeAssistantEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveHomeAssistantEndpointArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP federation — list_mcp_tools / run_mcp_tool
//
// These structs carry MCP protocol-level opaque blobs (input_schema, args,
// resource, structured_content) whose shapes are defined by upstream MCP
// servers, not by orca. serde_json::Value is the documented escape hatch.
// ═══════════════════════════════════════════════════════════════════════════

// Inner module so the #[allow] suppresses derives' expanded Value uses too.
#[allow(clippy::disallowed_types)]
mod mcp_fed {
    use super::*;

    /// `input_schema` is raw JSON Schema from the upstream MCP server — shape is server-defined.
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct McpToolEntry {
        pub server: String,
        pub name: String,
        pub description: String,
        /// Raw JSON Schema as advertised by the upstream MCP server — shape is server-defined.
        #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
        pub input_schema: serde_json::Value,
    }

    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[cfg_attr(feature = "cli", derive(clap::Args))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsArgs {}

    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsOutput {
        pub tools: Vec<McpToolEntry>,
    }

    /// `args` is passed straight through to the upstream MCP tool — its shape is
    /// dictated by each tool's own input schema and cannot be typed statically.
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct RunMcpToolArgs {
        /// Registered MCP server name.
        pub server: String,
        /// Tool name on the server (the internal name, not an orca alias).
        pub tool: String,
        /// JSON arguments object passed straight through to the tool.
        /// Opaque by the MCP protocol — shape is dictated by each tool's own input schema.
        #[serde(default)]
        #[cfg_attr(feature = "wasm", tsify(type = "Record<string, unknown> | null"))]
        pub args: Option<serde_json::Map<String, serde_json::Value>>,
    }

    /// One block in an MCP `tools/call` result's `content` array.
    ///
    /// MCP spec content kinds: `text` (carries `text`), `image` / `audio`
    /// (carry `data` base64 + `mime_type`), `resource` (carries `resource`).
    /// We preserve every shape: required fields are typed, optional ones are
    /// kept as opaque JSON so we never lose data.
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct McpContent {
        /// `"text" | "image" | "audio" | "resource"` per the MCP spec.
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mime_type: Option<String>,
        /// Opaque MCP `resource` content block — shape is server-defined per MCP spec.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
        pub resource: Option<Value>,
    }

    /// `structured_content` is opaque — its shape is each tool's own output schema,
    /// which orca cannot know at this layer (MCP passthrough).
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct RunMcpToolOutput {
        pub content: Vec<McpContent>,
        pub is_error: bool,
        /// Structured tool result if the server provided one alongside `content`
        /// (MCP `structuredContent`). Kept as opaque JSON — its shape is the
        /// tool's own output schema, which orca cannot know at this layer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
        pub structured_content: Option<Value>,
    }
}

pub use mcp_fed::{
    ListMcpToolsArgs, ListMcpToolsOutput, McpContent, McpToolEntry, RunMcpToolArgs,
    RunMcpToolOutput,
};

// ═══════════════════════════════════════════════════════════════════════════
// Schema view — get_schema / get_schema_domains
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaArgs {}

/// One row in `tabs[*].tables`.
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaTableInfo {
    pub name: String,
    pub comment: String,
}

/// One column entry within `tabs[*].columns[tableName]`.
///
/// Field names match what the HTTP `/api/schema` handler emits today (the
/// frontend reads `fk_target` snake_case directly).
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub nullable: bool,
    pub key: String,
    pub extra: String,
    pub fk_target: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaForeignKey {
    pub table: String,
    pub column: String,
    pub ref_table: String,
    pub ref_column: String,
}

/// Domain grouping (loaded from each schema DB's `domainsFile` JSON).
/// Optional fields (`group`, `subgroup`) are not always present.
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaDomain {
    pub key: String,
    pub label: String,
    pub color: String,
    pub tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subgroup: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaTab {
    pub title: String,
    pub tables: Vec<SchemaTableInfo>,
    pub columns: HashMap<String, Vec<SchemaColumn>>,
    pub foreign_keys: Vec<SchemaForeignKey>,
    pub domains: Vec<SchemaDomain>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetSchemaOutput {
    pub tabs: Vec<SchemaTab>,
    pub show_tabs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsOutput {
    pub domains: Vec<SchemaDomain>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Native dispatch helpers
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
use crate::services::mgmt as svc;

#[cfg(feature = "native")]
fn mcp(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::McpRegistryService>> {
    ctx.service::<std::sync::Arc<dyn svc::McpRegistryService>>()
}
#[cfg(feature = "native")]
fn sch(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::SchemaDbService>> {
    ctx.service::<std::sync::Arc<dyn svc::SchemaDbService>>()
}
#[cfg(feature = "native")]
fn drt(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::DockerRuntimeService>> {
    ctx.service::<std::sync::Arc<dyn svc::DockerRuntimeService>>()
}
#[cfg(feature = "native")]
fn doc(ctx: &orca_utils::tool::ToolCtx) -> anyhow::Result<std::sync::Arc<dyn svc::DocRootService>> {
    ctx.service::<std::sync::Arc<dyn svc::DocRootService>>()
}
#[cfg(feature = "native")]
fn pmx(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::ProxmoxEndpointService>> {
    ctx.service::<std::sync::Arc<dyn svc::ProxmoxEndpointService>>()
}
#[cfg(feature = "native")]
fn ha(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::HaEndpointService>> {
    ctx.service::<std::sync::Arc<dyn svc::HaEndpointService>>()
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP federation tools
// ═══════════════════════════════════════════════════════════════════════════

/// List every tool advertised by every registered MCP server (connects on demand).
#[orca_tool(domain = "mcp-federation", verb = "list-tools")]
async fn list_mcp_tools(
    _args: ListMcpToolsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListMcpToolsOutput> {
    let tools = mcp(ctx)?
        .list_tools()
        .await?
        .into_iter()
        .map(|t| McpToolEntry {
            server: t.server,
            name: t.name,
            description: t.description,
            input_schema: t.input_schema,
        })
        .collect();
    Ok(ListMcpToolsOutput { tools })
}

/// [MUTATES STATE] Invoke a tool on a registered MCP server. Returns the typed `tools/call` envelope (`{ content, isError, structuredContent? }`).
#[orca_tool(domain = "mcp-federation", verb = "run", cli = skip)]
async fn run_mcp_tool(
    args: RunMcpToolArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RunMcpToolOutput> {
    let arguments = match args.args {
        Some(m) => serde_json::Value::Object(m),
        None => serde_json::json!({}),
    };
    mcp(ctx)?
        .run_tool(&args.server, &args.tool, arguments)
        .await
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema view tools
// ═══════════════════════════════════════════════════════════════════════════

/// Return the multi-tab schema view across every configured database. Result is `{ tabs, showTabs, errors? }`.
#[orca_tool(domain = "schema-view", verb = "get")]
async fn get_schema(
    _args: GetSchemaArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetSchemaOutput> {
    sch(ctx)?.schema().await
}

/// Return the flattened list of domain definitions across every configured database.
#[orca_tool(domain = "schema-view", verb = "list-domains")]
async fn get_schema_domains(
    _args: GetSchemaDomainsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetSchemaDomainsOutput> {
    Ok(GetSchemaDomainsOutput {
        domains: sch(ctx)?.schema_domains().await?,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP servers + tool mappings
// ═══════════════════════════════════════════════════════════════════════════

/// List all MCP servers registered in orca.db (orca's own managed registry). Does not include ~/.claude.json servers managed by Claude Code directly.
#[orca_tool(domain = "mcp", verb = "list")]
async fn list_mcp_servers(
    _args: ListMcpServersArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListMcpServersOutput> {
    let servers = mcp(ctx)?
        .list_servers()
        .await?
        .into_iter()
        .map(|s| McpServerEntry {
            name: s.name,
            command: s.command,
            args: s.args,
            env: s.env,
            enabled: s.enabled,
        })
        .collect();
    Ok(ListMcpServersOutput { servers })
}

/// [MUTATES STATE] Add or update an MCP server in orca.db. Use when registering a new MCP server for orca to federate.
#[orca_tool(domain = "mcp", verb = "add")]
async fn add_mcp_server(
    args: AddMcpServerArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<McpServerMutationResult> {
    mcp(ctx)?
        .upsert_server(svc::McpServerInput {
            name: args.name.clone(),
            command: args.command,
            args: args.args.unwrap_or_default(),
            env: args.env.unwrap_or_default(),
        })
        .await?;
    Ok(McpServerMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove an MCP server from orca.db by name.
#[orca_tool(domain = "mcp", verb = "remove")]
async fn remove_mcp_server(
    args: RemoveMcpServerArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<McpServerMutationResult> {
    let changed = mcp(ctx)?.remove_server(&args.name).await?;
    Ok(McpServerMutationResult {
        name: args.name,
        changed,
    })
}

/// [MUTATES STATE] Map an orca tool name to a specific tool on a registered MCP server.
#[orca_tool(domain = "mcp", verb = "map")]
async fn map_tool(
    args: MapToolArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<MapToolResult> {
    mcp(ctx)?
        .map_tool(&args.name, &args.orca_tool, &args.external_tool)
        .await?;
    Ok(MapToolResult {
        orca_tool: args.orca_tool,
        mcp_name: args.name,
        external_tool: args.external_tool,
    })
}

/// [MUTATES STATE] Remove a tool mapping from orca.db.
#[orca_tool(domain = "mcp", verb = "unmap")]
async fn unmap_tool(
    args: UnmapToolArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UnmapToolResult> {
    let changed = mcp(ctx)?.unmap_tool(&args.orca_tool).await?;
    Ok(UnmapToolResult {
        orca_tool: args.orca_tool,
        changed,
    })
}

/// [MUTATES STATE] Auto-discover and map tools from registered MCP servers. Provide name or set all=true.
#[orca_tool(domain = "mcp", verb = "sync")]
async fn sync_tools(
    args: SyncToolsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SyncToolsOutput> {
    let threshold = args.threshold.unwrap_or(0.8);
    let results = mcp(ctx)?
        .sync_tools(args.all.unwrap_or(false), args.name.as_deref(), threshold)
        .await?
        .into_iter()
        .map(|r| SyncToolsServerEntry {
            server: r.server,
            added: r.added,
            skipped: r.skipped,
            error: r.error,
        })
        .collect();
    Ok(SyncToolsOutput { results })
}

/// List all tool mappings in orca.db, optionally filtered by server name.
#[orca_tool(domain = "mcp", verb = "list-mappings")]
async fn list_tool_mappings(
    args: ListToolMappingsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListToolMappingsOutput> {
    let mappings = mcp(ctx)?
        .list_mappings(args.name.as_deref())
        .await?
        .into_iter()
        .map(|m| MappingEntry {
            orca_tool: m.orca_tool,
            mcp_name: m.mcp_name,
            external_tool: m.external_tool,
            match_type: m.match_type,
            confidence: m.confidence,
            enabled: m.enabled,
        })
        .collect();
    Ok(ListToolMappingsOutput { mappings })
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema databases
// ═══════════════════════════════════════════════════════════════════════════

/// List all MySQL/MariaDB schema databases registered in orca.db.
#[orca_tool(domain = "schema", verb = "list")]
async fn list_schemas(
    _args: ListSchemasArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListSchemasOutput> {
    let schemas = sch(ctx)?
        .list()
        .await?
        .into_iter()
        .map(|d| SchemaDbEntry {
            name: d.name,
            driver: d.driver,
            host: d.host,
            port: d.port,
            user: d.user,
            database: d.database,
            container: d.container,
            domains_file: d.domains_file,
            enabled: d.enabled,
        })
        .collect();
    Ok(ListSchemasOutput { schemas })
}

/// [MUTATES STATE] Add or update a schema database in orca.db. Use container OR host/port, not both.
#[orca_tool(domain = "schema", verb = "add")]
async fn add_schema(
    args: AddSchemaArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SchemaMutationResult> {
    sch(ctx)?
        .upsert(svc::SchemaDbInput {
            name: args.name.clone(),
            database: args.database,
            user: args.user,
            password: args.password,
            container: args.container,
            host: args.host,
            port: args.port,
            domains_file: args.domains_file,
        })
        .await?;
    Ok(SchemaMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a schema database from orca.db by name.
#[orca_tool(domain = "schema", verb = "remove")]
async fn remove_schema(
    args: RemoveSchemaArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SchemaMutationResult> {
    let changed = sch(ctx)?.remove(&args.name).await?;
    Ok(SchemaMutationResult {
        name: args.name,
        changed,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Docker runtimes
// ═══════════════════════════════════════════════════════════════════════════

/// List all Docker runtimes registered in orca.db.
#[orca_tool(domain = "docker-runtime", verb = "list")]
async fn list_docker_runtimes(
    _args: ListDockerRuntimesArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListDockerRuntimesOutput> {
    let runtimes = drt(ctx)?
        .list()
        .await?
        .into_iter()
        .map(|r| DockerRuntimeEntry {
            name: r.name,
            socket_path: r.socket_path,
            host: r.host,
            url: r.url,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListDockerRuntimesOutput { runtimes })
}

/// [MUTATES STATE] Register a Docker runtime in orca.db. Provide socketPath, host, or url.
#[orca_tool(domain = "docker-runtime", verb = "add")]
async fn add_docker_runtime(
    args: AddDockerRuntimeArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DockerRuntimeMutationResult> {
    drt(ctx)?
        .upsert(svc::DockerRuntimeInput {
            name: args.name.clone(),
            socket_path: args.socket_path,
            host: args.host,
            url: args.url,
        })
        .await?;
    Ok(DockerRuntimeMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a Docker runtime from orca.db by name.
#[orca_tool(domain = "docker-runtime", verb = "remove")]
async fn remove_docker_runtime(
    args: RemoveDockerRuntimeArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DockerRuntimeMutationResult> {
    let changed = drt(ctx)?.remove(&args.name).await?;
    Ok(DockerRuntimeMutationResult {
        name: args.name,
        changed,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Doc roots + ignore patterns
// ═══════════════════════════════════════════════════════════════════════════

/// List all documentation roots registered in orca.db.
#[orca_tool(domain = "doc-root", verb = "list")]
async fn list_doc_roots(
    _args: ListDocRootsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListDocRootsOutput> {
    let roots = doc(ctx)?
        .list_roots()
        .await?
        .into_iter()
        .map(|r| DocRootRegEntry {
            name: r.name,
            path: r.path,
            description: r.description,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListDocRootsOutput { roots })
}

/// [MUTATES STATE] Register a documentation root directory in orca.db.
#[orca_tool(domain = "doc-root", verb = "add")]
async fn add_doc_root(
    args: AddDocRootArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DocRootMutationResult> {
    doc(ctx)?
        .upsert_root(svc::DocRootInput {
            name: args.name.clone(),
            path: args.path,
            description: args.description,
        })
        .await?;
    Ok(DocRootMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a documentation root from orca.db by name.
#[orca_tool(domain = "doc-root", verb = "remove")]
async fn remove_doc_root(
    args: RemoveDocRootArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DocRootMutationResult> {
    let changed = doc(ctx)?.remove_root(&args.name).await?;
    Ok(DocRootMutationResult {
        name: args.name,
        changed,
    })
}

/// List directory names excluded from all doc roots (e.g. node_modules, .git).
#[orca_tool(domain = "doc-pattern", verb = "list")]
async fn list_doc_ignore_patterns(
    _args: ListDocIgnorePatternsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListDocIgnorePatternsOutput> {
    let patterns = doc(ctx)?.list_ignore_patterns().await?;
    Ok(ListDocIgnorePatternsOutput { patterns })
}

/// [MUTATES STATE] Add a directory name to the global doc ignore list.
#[orca_tool(domain = "doc-pattern", verb = "add")]
async fn add_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DocIgnorePatternMutationResult> {
    let changed = doc(ctx)?.add_ignore_pattern(&args.pattern).await?;
    Ok(DocIgnorePatternMutationResult {
        pattern: args.pattern,
        changed,
    })
}

/// [MUTATES STATE] Remove a directory name from the global doc ignore list.
#[orca_tool(domain = "doc-pattern", verb = "remove")]
async fn remove_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DocIgnorePatternMutationResult> {
    let changed = doc(ctx)?.remove_ignore_pattern(&args.pattern).await?;
    Ok(DocIgnorePatternMutationResult {
        pattern: args.pattern,
        changed,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Proxmox endpoints
// ═══════════════════════════════════════════════════════════════════════════

/// List all Proxmox VE endpoints registered in orca.db (token secrets are redacted).
#[orca_tool(domain = "proxmox-endpoint", verb = "list")]
async fn list_proxmox_endpoints(
    _args: ListProxmoxEndpointsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListProxmoxEndpointsOutput> {
    let endpoints = pmx(ctx)?
        .list()
        .await?
        .into_iter()
        .map(|r| ProxmoxEndpointEntry {
            name: r.name,
            base_url: r.base_url,
            token_id: r.token_id,
            insecure: r.insecure,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListProxmoxEndpointsOutput { endpoints })
}

/// [MUTATES STATE] Register or update a Proxmox VE endpoint in orca.db. Auth uses an API token (PVEAPIToken header).
#[orca_tool(domain = "proxmox-endpoint", verb = "add")]
async fn add_proxmox_endpoint(
    args: AddProxmoxEndpointArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProxmoxMutationResult> {
    pmx(ctx)?
        .upsert(svc::ProxmoxEndpointInput {
            name: args.name.clone(),
            base_url: args.base_url,
            token_id: args.token_id,
            token_secret: args.token_secret,
            insecure: args.insecure.unwrap_or(false),
        })
        .await?;
    Ok(ProxmoxMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a Proxmox VE endpoint from orca.db by name.
#[orca_tool(domain = "proxmox-endpoint", verb = "remove")]
async fn remove_proxmox_endpoint(
    args: RemoveProxmoxEndpointArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProxmoxMutationResult> {
    let changed = pmx(ctx)?.remove(&args.name).await?;
    Ok(ProxmoxMutationResult {
        name: args.name,
        changed,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Home Assistant endpoints
// ═══════════════════════════════════════════════════════════════════════════

/// List all Home Assistant endpoints registered in orca.db (tokens are redacted).
#[orca_tool(domain = "ha-endpoint", verb = "list")]
async fn list_home_assistant_endpoints(
    _args: ListHomeAssistantEndpointsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListHomeAssistantEndpointsOutput> {
    let endpoints = ha(ctx)?
        .list()
        .await?
        .into_iter()
        .map(|r| HaEndpointEntry {
            name: r.name,
            base_url: r.base_url,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListHomeAssistantEndpointsOutput { endpoints })
}

/// [MUTATES STATE] Register or update a Home Assistant endpoint in orca.db. Auth uses a long-lived access token (Bearer header).
#[orca_tool(domain = "ha-endpoint", verb = "add")]
async fn add_home_assistant_endpoint(
    args: AddHomeAssistantEndpointArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HaMutationResult> {
    ha(ctx)?
        .upsert(svc::HaEndpointInput {
            name: args.name.clone(),
            base_url: args.base_url,
            token: args.token,
        })
        .await?;
    Ok(HaMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove a Home Assistant endpoint from orca.db by name.
#[orca_tool(domain = "ha-endpoint", verb = "remove")]
async fn remove_home_assistant_endpoint(
    args: RemoveHomeAssistantEndpointArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<HaMutationResult> {
    let changed = ha(ctx)?.remove(&args.name).await?;
    Ok(HaMutationResult {
        name: args.name,
        changed,
    })
}
