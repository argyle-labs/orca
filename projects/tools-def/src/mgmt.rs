//! Management domain tools — MCP server registry + tool mappings, schema
//! databases, Docker runtimes, doc roots + ignore patterns, Proxmox + Home
//! Assistant endpoints. Run impls dispatch through the six sub-services in
//! `services::mgmt`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::OrcaToolDef;
use serde_json::Value;

// ═══════════════════════════════════════════════════════════════════════════
// MCP servers + tool mappings
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersOutput {
    pub servers: Vec<McpServerEntry>,
}

pub struct ListMcpServers;
impl OrcaToolDef for ListMcpServers {
    const NAME: &'static str = "list_mcp_servers";
    const DESCRIPTION: &'static str = "List all MCP servers registered in orca.db (orca's own \
         managed registry). Does not include ~/.claude.json servers managed by Claude Code directly.";
    type Args = ListMcpServersArgs;
    type Output = ListMcpServersOutput;
}

// ── add_mcp_server ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddMcpServerArgs {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddMcpServer;
impl OrcaToolDef for AddMcpServer {
    const NAME: &'static str = "add_mcp_server";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Add or update an MCP server in orca.db. \
         Use when registering a new MCP server for orca to federate.";
    type Args = AddMcpServerArgs;
    type Output = McpServerMutationResult;
}

// ── remove_mcp_server ───────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveMcpServerArgs {
    pub name: String,
}

pub struct RemoveMcpServer;
impl OrcaToolDef for RemoveMcpServer {
    const NAME: &'static str = "remove_mcp_server";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove an MCP server from orca.db by name.";
    type Args = RemoveMcpServerArgs;
    type Output = McpServerMutationResult;
}

// ── map_tool / unmap_tool ───────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MapToolArgs {
    pub name: String,
    pub orca_tool: String,
    pub external_tool: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MapToolResult {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
}

pub struct MapTool;
impl OrcaToolDef for MapTool {
    const NAME: &'static str = "map_tool";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Map an orca tool name to a specific tool on a registered MCP server.";
    type Args = MapToolArgs;
    type Output = MapToolResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolArgs {
    pub orca_tool: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolResult {
    pub orca_tool: String,
    pub changed: bool,
}

pub struct UnmapTool;
impl OrcaToolDef for UnmapTool {
    const NAME: &'static str = "unmap_tool";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove a tool mapping from orca.db.";
    type Args = UnmapToolArgs;
    type Output = UnmapToolResult;
}

// ── sync_tools ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsOutput {
    pub results: Vec<SyncToolsServerEntry>,
}

pub struct SyncTools;
impl OrcaToolDef for SyncTools {
    const NAME: &'static str = "sync_tools";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Auto-discover and map tools from registered \
         MCP servers. Provide name or set all=true.";
    type Args = SyncToolsArgs;
    type Output = SyncToolsOutput;
}

// ── list_tool_mappings ──────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsArgs {
    /// Filter by server name (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsOutput {
    pub mappings: Vec<MappingEntry>,
}

pub struct ListToolMappings;
impl OrcaToolDef for ListToolMappings {
    const NAME: &'static str = "list_tool_mappings";
    const DESCRIPTION: &'static str =
        "List all tool mappings in orca.db, optionally filtered by server name.";
    type Args = ListToolMappingsArgs;
    type Output = ListToolMappingsOutput;
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema databases
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasOutput {
    pub schemas: Vec<SchemaDbEntry>,
}

pub struct ListSchemas;
impl OrcaToolDef for ListSchemas {
    const NAME: &'static str = "list_schemas";
    const DESCRIPTION: &'static str =
        "List all MySQL/MariaDB schema databases registered in orca.db.";
    type Args = ListSchemasArgs;
    type Output = ListSchemasOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddSchema;
impl OrcaToolDef for AddSchema {
    const NAME: &'static str = "add_schema";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Add or update a schema database in orca.db. \
         Use container OR host/port, not both.";
    type Args = AddSchemaArgs;
    type Output = SchemaMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveSchemaArgs {
    pub name: String,
}

pub struct RemoveSchema;
impl OrcaToolDef for RemoveSchema {
    const NAME: &'static str = "remove_schema";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a schema database from orca.db by name.";
    type Args = RemoveSchemaArgs;
    type Output = SchemaMutationResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Docker runtimes
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDockerRuntimesArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDockerRuntimesOutput {
    pub runtimes: Vec<DockerRuntimeEntry>,
}

pub struct ListDockerRuntimes;
impl OrcaToolDef for ListDockerRuntimes {
    const NAME: &'static str = "list_docker_runtimes";
    const DESCRIPTION: &'static str = "List all Docker runtimes registered in orca.db.";
    type Args = ListDockerRuntimesArgs;
    type Output = ListDockerRuntimesOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DockerRuntimeMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddDockerRuntime;
impl OrcaToolDef for AddDockerRuntime {
    const NAME: &'static str = "add_docker_runtime";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register a Docker runtime in orca.db. \
         Provide socketPath, host, or url.";
    type Args = AddDockerRuntimeArgs;
    type Output = DockerRuntimeMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDockerRuntimeArgs {
    pub name: String,
}

pub struct RemoveDockerRuntime;
impl OrcaToolDef for RemoveDockerRuntime {
    const NAME: &'static str = "remove_docker_runtime";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Docker runtime from orca.db by name.";
    type Args = RemoveDockerRuntimeArgs;
    type Output = DockerRuntimeMutationResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Doc roots + ignore patterns
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootRegEntry {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsOutput {
    pub roots: Vec<DocRootRegEntry>,
}

pub struct ListDocRoots;
impl OrcaToolDef for ListDocRoots {
    const NAME: &'static str = "list_doc_roots";
    const DESCRIPTION: &'static str = "List all documentation roots registered in orca.db.";
    type Args = ListDocRootsArgs;
    type Output = ListDocRootsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddDocRootArgs {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddDocRoot;
impl OrcaToolDef for AddDocRoot {
    const NAME: &'static str = "add_doc_root";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Register a documentation root directory in orca.db.";
    type Args = AddDocRootArgs;
    type Output = DocRootMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDocRootArgs {
    pub name: String,
}

pub struct RemoveDocRoot;
impl OrcaToolDef for RemoveDocRoot {
    const NAME: &'static str = "remove_doc_root";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a documentation root from orca.db by name.";
    type Args = RemoveDocRootArgs;
    type Output = DocRootMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsOutput {
    pub patterns: Vec<String>,
}

pub struct ListDocIgnorePatterns;
impl OrcaToolDef for ListDocIgnorePatterns {
    const NAME: &'static str = "list_doc_ignore_patterns";
    const DESCRIPTION: &'static str =
        "List directory names excluded from all doc roots (e.g. node_modules, .git).";
    type Args = ListDocIgnorePatternsArgs;
    type Output = ListDocIgnorePatternsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternArgs {
    pub pattern: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternMutationResult {
    pub pattern: String,
    pub changed: bool,
}

pub struct AddDocIgnorePattern;
impl OrcaToolDef for AddDocIgnorePattern {
    const NAME: &'static str = "add_doc_ignore_pattern";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Add a directory name to the global doc ignore list.";
    type Args = DocIgnorePatternArgs;
    type Output = DocIgnorePatternMutationResult;
}

pub struct RemoveDocIgnorePattern;
impl OrcaToolDef for RemoveDocIgnorePattern {
    const NAME: &'static str = "remove_doc_ignore_pattern";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a directory name from the global doc ignore list.";
    type Args = DocIgnorePatternArgs;
    type Output = DocIgnorePatternMutationResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Proxmox endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsOutput {
    pub endpoints: Vec<ProxmoxEndpointEntry>,
}

pub struct ListProxmoxEndpoints;
impl OrcaToolDef for ListProxmoxEndpoints {
    const NAME: &'static str = "list_proxmox_endpoints";
    const DESCRIPTION: &'static str =
        "List all Proxmox VE endpoints registered in orca.db (token secrets are redacted).";
    type Args = ListProxmoxEndpointsArgs;
    type Output = ListProxmoxEndpointsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddProxmoxEndpoint;
impl OrcaToolDef for AddProxmoxEndpoint {
    const NAME: &'static str = "add_proxmox_endpoint";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register or update a Proxmox VE endpoint in \
         orca.db. Auth uses an API token (PVEAPIToken header).";
    type Args = AddProxmoxEndpointArgs;
    type Output = ProxmoxMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveProxmoxEndpointArgs {
    pub name: String,
}

pub struct RemoveProxmoxEndpoint;
impl OrcaToolDef for RemoveProxmoxEndpoint {
    const NAME: &'static str = "remove_proxmox_endpoint";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Proxmox VE endpoint from orca.db by name.";
    type Args = RemoveProxmoxEndpointArgs;
    type Output = ProxmoxMutationResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Home Assistant endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HaEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsOutput {
    pub endpoints: Vec<HaEndpointEntry>,
}

pub struct ListHomeAssistantEndpoints;
impl OrcaToolDef for ListHomeAssistantEndpoints {
    const NAME: &'static str = "list_home_assistant_endpoints";
    const DESCRIPTION: &'static str =
        "List all Home Assistant endpoints registered in orca.db (tokens are redacted).";
    type Args = ListHomeAssistantEndpointsArgs;
    type Output = ListHomeAssistantEndpointsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddHomeAssistantEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaMutationResult {
    pub name: String,
    pub changed: bool,
}

pub struct AddHomeAssistantEndpoint;
impl OrcaToolDef for AddHomeAssistantEndpoint {
    const NAME: &'static str = "add_home_assistant_endpoint";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register or update a Home Assistant endpoint \
         in orca.db. Auth uses a long-lived access token (Bearer header).";
    type Args = AddHomeAssistantEndpointArgs;
    type Output = HaMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveHomeAssistantEndpointArgs {
    pub name: String,
}

pub struct RemoveHomeAssistantEndpoint;
impl OrcaToolDef for RemoveHomeAssistantEndpoint {
    const NAME: &'static str = "remove_home_assistant_endpoint";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Home Assistant endpoint from orca.db by name.";
    type Args = RemoveHomeAssistantEndpointArgs;
    type Output = HaMutationResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP federation — list_mcp_tools / run_mcp_tool
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpToolEntry {
    pub server: String,
    pub name: String,
    pub description: String,
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    pub input_schema: serde_json::Value,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpToolsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpToolsOutput {
    pub tools: Vec<McpToolEntry>,
}

pub struct ListMcpTools;
impl OrcaToolDef for ListMcpTools {
    const NAME: &'static str = "list_mcp_tools";
    const DESCRIPTION: &'static str =
        "List every tool advertised by every registered MCP server (connects on demand).";
    type Args = ListMcpToolsArgs;
    type Output = ListMcpToolsOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RunMcpToolArgs {
    /// Registered MCP server name.
    pub server: String,
    /// Tool name on the server (the internal name, not an orca alias).
    pub tool: String,
    /// JSON arguments object passed straight through to the tool.
    #[serde(default)]
    #[cfg_attr(feature = "wasm", tsify(type = "Record<string, unknown> | null"))]
    pub args: Option<serde_json::Map<String, serde_json::Value>>,
}

pub struct RunMcpTool;
impl OrcaToolDef for RunMcpTool {
    const NAME: &'static str = "run_mcp_tool";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Invoke a tool on a registered MCP server. \
         Returns the raw `tools/call` result (typically `{ content: [...], isError? }`).";
    type Args = RunMcpToolArgs;
    type Output = crate::JsonAny;
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema view — get_schema / get_schema_domains
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaArgs {}

/// One row in `tabs[*].tables`.
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetSchemaOutput {
    pub tabs: Vec<SchemaTab>,
    pub show_tabs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

pub struct GetSchema;
impl OrcaToolDef for GetSchema {
    const NAME: &'static str = "get_schema";
    const DESCRIPTION: &'static str = "Return the multi-tab schema view across every configured \
         database. Result is `{ tabs, showTabs, errors? }`.";
    type Args = GetSchemaArgs;
    type Output = GetSchemaOutput;
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsOutput {
    pub domains: Vec<SchemaDomain>,
}

pub struct GetSchemaDomains;
impl OrcaToolDef for GetSchemaDomains {
    const NAME: &'static str = "get_schema_domains";
    const DESCRIPTION: &'static str = "Return the flattened list of domain definitions across every \
         configured database.";
    type Args = GetSchemaDomainsArgs;
    type Output = GetSchemaDomainsOutput;
}

// ═══════════════════════════════════════════════════════════════════════════
// Native run impls
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::mgmt as svc;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn mcp(ctx: &ToolCtx) -> Result<Arc<dyn svc::McpRegistryService>> {
        ctx.service::<Arc<dyn svc::McpRegistryService>>()
    }
    fn sch(ctx: &ToolCtx) -> Result<Arc<dyn svc::SchemaDbService>> {
        ctx.service::<Arc<dyn svc::SchemaDbService>>()
    }
    fn drt(ctx: &ToolCtx) -> Result<Arc<dyn svc::DockerRuntimeService>> {
        ctx.service::<Arc<dyn svc::DockerRuntimeService>>()
    }
    fn doc(ctx: &ToolCtx) -> Result<Arc<dyn svc::DocRootService>> {
        ctx.service::<Arc<dyn svc::DocRootService>>()
    }
    fn pmx(ctx: &ToolCtx) -> Result<Arc<dyn svc::ProxmoxEndpointService>> {
        ctx.service::<Arc<dyn svc::ProxmoxEndpointService>>()
    }
    fn ha(ctx: &ToolCtx) -> Result<Arc<dyn svc::HaEndpointService>> {
        ctx.service::<Arc<dyn svc::HaEndpointService>>()
    }

    // ── MCP servers + mappings ─────────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListMcpServers {
        async fn run(_args: ListMcpServersArgs, ctx: &ToolCtx) -> Result<ListMcpServersOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddMcpServer {
        async fn run(args: AddMcpServerArgs, ctx: &ToolCtx) -> Result<McpServerMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveMcpServer {
        async fn run(args: RemoveMcpServerArgs, ctx: &ToolCtx) -> Result<McpServerMutationResult> {
            let changed = mcp(ctx)?.remove_server(&args.name).await?;
            Ok(McpServerMutationResult {
                name: args.name,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for MapTool {
        async fn run(args: MapToolArgs, ctx: &ToolCtx) -> Result<MapToolResult> {
            mcp(ctx)?
                .map_tool(&args.name, &args.orca_tool, &args.external_tool)
                .await?;
            Ok(MapToolResult {
                orca_tool: args.orca_tool,
                mcp_name: args.name,
                external_tool: args.external_tool,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for UnmapTool {
        async fn run(args: UnmapToolArgs, ctx: &ToolCtx) -> Result<UnmapToolResult> {
            let changed = mcp(ctx)?.unmap_tool(&args.orca_tool).await?;
            Ok(UnmapToolResult {
                orca_tool: args.orca_tool,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for SyncTools {
        async fn run(args: SyncToolsArgs, ctx: &ToolCtx) -> Result<SyncToolsOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for ListMcpTools {
        async fn run(_args: ListMcpToolsArgs, ctx: &ToolCtx) -> Result<ListMcpToolsOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for RunMcpTool {
        async fn run(args: RunMcpToolArgs, ctx: &ToolCtx) -> Result<crate::JsonAny> {
            let arguments = match args.args {
                Some(m) => serde_json::Value::Object(m),
                None => serde_json::json!({}),
            };
            let result = mcp(ctx)?
                .run_tool(&args.server, &args.tool, arguments)
                .await?;
            Ok(result.into())
        }
    }

    #[async_trait]
    impl OrcaTool for GetSchema {
        async fn run(_args: GetSchemaArgs, ctx: &ToolCtx) -> Result<crate::JsonAny> {
            Ok(sch(ctx)?.schema().await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for GetSchemaDomains {
        async fn run(_args: GetSchemaDomainsArgs, ctx: &ToolCtx) -> Result<crate::JsonAny> {
            Ok(sch(ctx)?.schema_domains().await?.into())
        }
    }

    #[async_trait]
    impl OrcaTool for ListToolMappings {
        async fn run(args: ListToolMappingsArgs, ctx: &ToolCtx) -> Result<ListToolMappingsOutput> {
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
    }

    // ── Schemas ────────────────────────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListSchemas {
        async fn run(_args: ListSchemasArgs, ctx: &ToolCtx) -> Result<ListSchemasOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddSchema {
        async fn run(args: AddSchemaArgs, ctx: &ToolCtx) -> Result<SchemaMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveSchema {
        async fn run(args: RemoveSchemaArgs, ctx: &ToolCtx) -> Result<SchemaMutationResult> {
            let changed = sch(ctx)?.remove(&args.name).await?;
            Ok(SchemaMutationResult {
                name: args.name,
                changed,
            })
        }
    }

    // ── Docker runtimes ────────────────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListDockerRuntimes {
        async fn run(
            _args: ListDockerRuntimesArgs,
            ctx: &ToolCtx,
        ) -> Result<ListDockerRuntimesOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddDockerRuntime {
        async fn run(
            args: AddDockerRuntimeArgs,
            ctx: &ToolCtx,
        ) -> Result<DockerRuntimeMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveDockerRuntime {
        async fn run(
            args: RemoveDockerRuntimeArgs,
            ctx: &ToolCtx,
        ) -> Result<DockerRuntimeMutationResult> {
            let changed = drt(ctx)?.remove(&args.name).await?;
            Ok(DockerRuntimeMutationResult {
                name: args.name,
                changed,
            })
        }
    }

    // ── Doc roots + ignore patterns ────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListDocRoots {
        async fn run(_args: ListDocRootsArgs, ctx: &ToolCtx) -> Result<ListDocRootsOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddDocRoot {
        async fn run(args: AddDocRootArgs, ctx: &ToolCtx) -> Result<DocRootMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveDocRoot {
        async fn run(args: RemoveDocRootArgs, ctx: &ToolCtx) -> Result<DocRootMutationResult> {
            let changed = doc(ctx)?.remove_root(&args.name).await?;
            Ok(DocRootMutationResult {
                name: args.name,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for ListDocIgnorePatterns {
        async fn run(
            _args: ListDocIgnorePatternsArgs,
            ctx: &ToolCtx,
        ) -> Result<ListDocIgnorePatternsOutput> {
            let patterns = doc(ctx)?.list_ignore_patterns().await?;
            Ok(ListDocIgnorePatternsOutput { patterns })
        }
    }

    #[async_trait]
    impl OrcaTool for AddDocIgnorePattern {
        async fn run(
            args: DocIgnorePatternArgs,
            ctx: &ToolCtx,
        ) -> Result<DocIgnorePatternMutationResult> {
            let changed = doc(ctx)?.add_ignore_pattern(&args.pattern).await?;
            Ok(DocIgnorePatternMutationResult {
                pattern: args.pattern,
                changed,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for RemoveDocIgnorePattern {
        async fn run(
            args: DocIgnorePatternArgs,
            ctx: &ToolCtx,
        ) -> Result<DocIgnorePatternMutationResult> {
            let changed = doc(ctx)?.remove_ignore_pattern(&args.pattern).await?;
            Ok(DocIgnorePatternMutationResult {
                pattern: args.pattern,
                changed,
            })
        }
    }

    // ── Proxmox endpoints ──────────────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListProxmoxEndpoints {
        async fn run(
            _args: ListProxmoxEndpointsArgs,
            ctx: &ToolCtx,
        ) -> Result<ListProxmoxEndpointsOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddProxmoxEndpoint {
        async fn run(args: AddProxmoxEndpointArgs, ctx: &ToolCtx) -> Result<ProxmoxMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveProxmoxEndpoint {
        async fn run(
            args: RemoveProxmoxEndpointArgs,
            ctx: &ToolCtx,
        ) -> Result<ProxmoxMutationResult> {
            let changed = pmx(ctx)?.remove(&args.name).await?;
            Ok(ProxmoxMutationResult {
                name: args.name,
                changed,
            })
        }
    }

    // ── Home Assistant endpoints ───────────────────────────────────────────

    #[async_trait]
    impl OrcaTool for ListHomeAssistantEndpoints {
        async fn run(
            _args: ListHomeAssistantEndpointsArgs,
            ctx: &ToolCtx,
        ) -> Result<ListHomeAssistantEndpointsOutput> {
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
    }

    #[async_trait]
    impl OrcaTool for AddHomeAssistantEndpoint {
        async fn run(
            args: AddHomeAssistantEndpointArgs,
            ctx: &ToolCtx,
        ) -> Result<HaMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for RemoveHomeAssistantEndpoint {
        async fn run(
            args: RemoveHomeAssistantEndpointArgs,
            ctx: &ToolCtx,
        ) -> Result<HaMutationResult> {
            let changed = ha(ctx)?.remove(&args.name).await?;
            Ok(HaMutationResult {
                name: args.name,
                changed,
            })
        }
    }
}
