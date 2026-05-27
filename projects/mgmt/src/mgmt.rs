//! Management domain tools — MCP server registry + tool mappings, schema
//! databases, Docker runtimes, doc roots + ignore patterns, Proxmox + Home
//! Assistant endpoints. Run impls dispatch through the six sub-services in
//! `services::mgmt`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use orca_macro::orca_tool;
// Value is used only in MCP-federation inner modules where all Value uses are
// legitimate opaque blobs (MCP protocol-level). The allow on each mod block
// covers derive expansions; this import-level allow covers the import itself.
#[allow(clippy::disallowed_types)]
use serde_json::Value;

// ═══════════════════════════════════════════════════════════════════════════
// MCP servers + tool mappings — shared row shapes
// ═══════════════════════════════════════════════════════════════════════════

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

// ── list_mcp_servers ────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListMcpServersOutput {
    pub servers: Vec<McpServerEntry>,
}

// ── add_mcp_server ──────────────────────────────────────────────────────────

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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerMutationResult {
    pub name: String,
    pub changed: bool,
}

// ── remove_mcp_server ───────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveMcpServerArgs {
    pub name: String,
}

// ── map_tool / unmap_tool ───────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolArgs {
    pub orca_tool: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnmapToolResult {
    pub orca_tool: String,
    pub changed: bool,
}

// ── sync_tools ──────────────────────────────────────────────────────────────

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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncToolsOutput {
    pub results: Vec<SyncToolsServerEntry>,
}

// ── list_tool_mappings ──────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsArgs {
    /// Filter by server name (omit for all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListToolMappingsOutput {
    pub mappings: Vec<MappingEntry>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Schema databases
// ═══════════════════════════════════════════════════════════════════════════

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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSchemasOutput {
    pub schemas: Vec<SchemaDbEntry>,
}

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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveSchemaArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Doc roots + ignore patterns
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootRegEntry {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocRootsOutput {
    pub roots: Vec<DocRootRegEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AddDocRootArgs {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveDocRootArgs {
    pub name: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsOutput {
    pub patterns: Vec<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternArgs {
    pub pattern: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocIgnorePatternMutationResult {
    pub pattern: String,
    pub changed: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// Proxmox endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProxmoxEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub insecure: bool,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsOutput {
    pub endpoints: Vec<ProxmoxEndpointEntry>,
}

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

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProxmoxMutationResult {
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RemoveProxmoxEndpointArgs {
    pub name: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Home Assistant endpoints
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HaEndpointEntry {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsOutput {
    pub endpoints: Vec<HaEndpointEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddHomeAssistantEndpointArgs {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct HaMutationResult {
    pub name: String,
    pub changed: bool,
}

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
    use ::platform::json_schema::JsonSchemaNode;

    /// `input_schema` is JSON Schema from the upstream MCP server, typed via
    /// `JsonSchemaNode` (full vocabulary + recursive typed extensions; no `Value` leak).
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct McpToolEntry {
        pub server: String,
        pub name: String,
        pub description: String,
        /// Typed JSON Schema as advertised by the upstream MCP server.
        pub input_schema: JsonSchemaNode,
    }

    #[cfg_attr(feature = "cli", derive(clap::Args))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsArgs {}

    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ListMcpToolsOutput {
        pub tools: Vec<McpToolEntry>,
    }

    /// `args` is passed straight through to the upstream MCP tool — its shape is
    /// dictated by each tool's own input schema and cannot be typed statically.
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct RunMcpToolArgs {
        /// Registered MCP server name.
        pub server: String,
        /// Tool name on the server (the internal name, not an orca alias).
        pub tool: String,
        /// JSON arguments object passed straight through to the tool.
        /// Opaque by the MCP protocol — shape is dictated by each tool's own input schema.
        #[serde(default)]
        pub args: Option<serde_json::Map<String, serde_json::Value>>,
    }

    /// One block in an MCP `tools/call` result's `content` array.
    ///
    /// MCP spec content kinds: `text` (carries `text`), `image` / `audio`
    /// (carry `data` base64 + `mime_type`), `resource` (carries `resource`).
    /// We preserve every shape: required fields are typed, optional ones are
    /// kept as opaque JSON so we never lose data.
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
        pub resource: Option<Value>,
    }

    /// `structured_content` is opaque — its shape is each tool's own output schema,
    /// which orca cannot know at this layer (MCP passthrough).
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct RunMcpToolOutput {
        pub content: Vec<McpContent>,
        pub is_error: bool,
        /// Structured tool result if the server provided one alongside `content`
        /// (MCP `structuredContent`). Kept as opaque JSON — its shape is the
        /// tool's own output schema, which orca cannot know at this layer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub structured_content: Option<Value>,
    }
}

pub use mcp_fed::{
    ListMcpToolsArgs, ListMcpToolsOutput, McpContent, McpToolEntry, RunMcpToolArgs,
    RunMcpToolOutput,
};

// ═══════════════════════════════════════════════════════════════════════════
// Schema view — schema_view_detail / schema_view_list
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaArgs {}

/// One row in `tabs[*].tables`.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SchemaTableInfo {
    pub name: String,
    pub comment: String,
}

/// One column entry within `tabs[*].columns[tableName]`.
///
/// Field names match what the HTTP `/api/schema` handler emits today (the
/// frontend reads `fk_target` snake_case directly).
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

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SchemaTab {
    pub title: String,
    pub tables: Vec<SchemaTableInfo>,
    pub columns: HashMap<String, Vec<SchemaColumn>>,
    pub foreign_keys: Vec<SchemaForeignKey>,
    pub domains: Vec<SchemaDomain>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetSchemaOutput {
    pub tabs: Vec<SchemaTab>,
    pub show_tabs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSchemaDomainsOutput {
    pub domains: Vec<SchemaDomain>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Native dispatch helpers
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
use crate::mgmt as svc;

#[cfg(feature = "native")]
fn mcp(
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::McpRegistryService>> {
    ctx.service::<std::sync::Arc<dyn svc::McpRegistryService>>()
}
#[cfg(feature = "native")]
fn sch(ctx: &orca_contract::ToolCtx) -> anyhow::Result<std::sync::Arc<dyn svc::SchemaDbService>> {
    ctx.service::<std::sync::Arc<dyn svc::SchemaDbService>>()
}
#[cfg(feature = "native")]
fn drt(
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::DockerRuntimeService>> {
    ctx.service::<std::sync::Arc<dyn svc::DockerRuntimeService>>()
}
#[cfg(feature = "native")]
fn doc(ctx: &orca_contract::ToolCtx) -> anyhow::Result<std::sync::Arc<dyn svc::DocRootService>> {
    ctx.service::<std::sync::Arc<dyn svc::DocRootService>>()
}
#[cfg(feature = "native")]
fn pmx(
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn svc::ProxmoxEndpointService>> {
    ctx.service::<std::sync::Arc<dyn svc::ProxmoxEndpointService>>()
}
#[cfg(feature = "native")]
fn ha(ctx: &orca_contract::ToolCtx) -> anyhow::Result<std::sync::Arc<dyn svc::HaEndpointService>> {
    ctx.service::<std::sync::Arc<dyn svc::HaEndpointService>>()
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP federation tools
// ═══════════════════════════════════════════════════════════════════════════

/// List every tool advertised by every registered MCP server (connects on demand).
#[orca_tool(domain = "system.mcp.federation", verb = "list-tools")]
async fn list_mcp_tools(
    _args: ListMcpToolsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "system.mcp.federation", verb = "run", cli = skip)]
async fn run_mcp_tool(
    args: RunMcpToolArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.schema.view", verb = "detail")]
async fn schema_view_detail(
    _args: GetSchemaArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetSchemaOutput> {
    sch(ctx)?.schema().await
}

/// Return the flattened list of domain definitions across every configured database.
#[orca_tool(domain = "namespace.schema.view", verb = "list")]
async fn schema_view_list(
    _args: GetSchemaDomainsArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetSchemaDomainsOutput> {
    Ok(GetSchemaDomainsOutput {
        domains: sch(ctx)?.schema_domains().await?,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// MCP servers + tool mappings
// ═══════════════════════════════════════════════════════════════════════════

/// List all MCP servers registered in orca.db (orca's own managed registry). Does not include ~/.claude.json servers managed by Claude Code directly.
#[orca_tool(domain = "system.mcp", verb = "list")]
async fn list_mcp_servers(
    _args: ListMcpServersArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "system.mcp", verb = "create")]
async fn add_mcp_server(
    args: AddMcpServerArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "system.mcp", verb = "delete")]
async fn remove_mcp_server(
    args: RemoveMcpServerArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<McpServerMutationResult> {
    let changed = mcp(ctx)?.remove_server(&args.name).await?;
    Ok(McpServerMutationResult {
        name: args.name,
        changed,
    })
}

/// [MUTATES STATE] Map an orca tool name to a specific tool on a registered MCP server.
#[orca_tool(domain = "system.mcp.mapping", verb = "create")]
async fn mcp_mapping_create(
    args: MapToolArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "system.mcp.mapping", verb = "delete")]
async fn mcp_mapping_delete(
    args: UnmapToolArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<UnmapToolResult> {
    let changed = mcp(ctx)?.unmap_tool(&args.orca_tool).await?;
    Ok(UnmapToolResult {
        orca_tool: args.orca_tool,
        changed,
    })
}

/// [MUTATES STATE] Auto-discover and map tools from registered MCP servers. Provide name or set all=true.
#[orca_tool(domain = "system.mcp", verb = "sync")]
async fn sync_tools(
    args: SyncToolsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "system.mcp.mapping", verb = "list")]
async fn mcp_mapping_list(
    args: ListToolMappingsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.schema", verb = "list")]
async fn list_schemas(
    _args: ListSchemasArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.schema", verb = "create")]
async fn add_schema(
    args: AddSchemaArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.schema", verb = "delete")]
async fn remove_schema(
    args: RemoveSchemaArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "docker.runtime", verb = "list")]
async fn list_docker_runtimes(
    _args: ListDockerRuntimesArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "docker.runtime", verb = "create")]
async fn add_docker_runtime(
    args: AddDockerRuntimeArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "docker.runtime", verb = "delete")]
async fn remove_docker_runtime(
    args: RemoveDockerRuntimeArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.doc.root", verb = "list")]
async fn list_doc_roots(
    _args: ListDocRootsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.doc.root", verb = "create")]
async fn add_doc_root(
    args: AddDocRootArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "namespace.doc.root", verb = "delete")]
async fn remove_doc_root(
    args: RemoveDocRootArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocRootMutationResult> {
    let changed = doc(ctx)?.remove_root(&args.name).await?;
    Ok(DocRootMutationResult {
        name: args.name,
        changed,
    })
}

/// List directory names excluded from all doc roots (e.g. node_modules, .git).
#[orca_tool(domain = "namespace.doc.pattern", verb = "list")]
async fn list_doc_ignore_patterns(
    _args: ListDocIgnorePatternsArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListDocIgnorePatternsOutput> {
    let patterns = doc(ctx)?.list_ignore_patterns().await?;
    Ok(ListDocIgnorePatternsOutput { patterns })
}

/// [MUTATES STATE] Add a directory name to the global doc ignore list.
#[orca_tool(domain = "namespace.doc.pattern", verb = "create")]
async fn add_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DocIgnorePatternMutationResult> {
    let changed = doc(ctx)?.add_ignore_pattern(&args.pattern).await?;
    Ok(DocIgnorePatternMutationResult {
        pattern: args.pattern,
        changed,
    })
}

/// [MUTATES STATE] Remove a directory name from the global doc ignore list.
#[orca_tool(domain = "namespace.doc.pattern", verb = "delete")]
async fn remove_doc_ignore_pattern(
    args: DocIgnorePatternArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "proxmox.endpoint", verb = "list")]
async fn list_proxmox_endpoints(
    _args: ListProxmoxEndpointsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "proxmox.endpoint", verb = "create")]
async fn add_proxmox_endpoint(
    args: AddProxmoxEndpointArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "proxmox.endpoint", verb = "delete")]
async fn remove_proxmox_endpoint(
    args: RemoveProxmoxEndpointArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "ha.endpoint", verb = "list")]
async fn list_home_assistant_endpoints(
    _args: ListHomeAssistantEndpointsArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "ha.endpoint", verb = "create")]
async fn add_home_assistant_endpoint(
    args: AddHomeAssistantEndpointArgs,
    ctx: &orca_contract::ToolCtx,
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
#[orca_tool(domain = "ha.endpoint", verb = "delete")]
async fn remove_home_assistant_endpoint(
    args: RemoveHomeAssistantEndpointArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<HaMutationResult> {
    let changed = ha(ctx)?.remove(&args.name).await?;
    Ok(HaMutationResult {
        name: args.name,
        changed,
    })
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

// ── MCP servers + tool mappings ─────────────────────────────────────────────

#[derive(Clone)]
pub struct McpServerData {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct McpServerInput {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Clone)]
pub struct ToolMappingData {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    pub confidence: Option<f64>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct SyncToolsServerResult {
    pub server: String,
    pub added: u32,
    pub skipped: u32,
    pub error: Option<String>,
}

/// Live tool description from a federated MCP server. Returned by
/// `McpRegistryService::list_tools` — surfaces the union of all tools currently
/// exposed by every registered server (connecting on demand).
#[derive(Clone)]
pub struct McpToolMeta {
    pub server: String,
    pub name: String,
    pub description: String,
    /// Typed JSON Schema from the upstream MCP server.
    pub input_schema: ::platform::json_schema::JsonSchemaNode,
}

#[async_trait]
pub trait McpRegistryService: Send + Sync {
    async fn list_servers(&self) -> Result<Vec<McpServerData>>;
    async fn upsert_server(&self, input: McpServerInput) -> Result<()>;
    async fn remove_server(&self, name: &str) -> Result<bool>;

    async fn map_tool(&self, name: &str, orca_tool: &str, external_tool: &str) -> Result<()>;
    async fn unmap_tool(&self, orca_tool: &str) -> Result<bool>;

    /// `all=true` syncs every registered server; otherwise `name` must be set.
    async fn sync_tools(
        &self,
        all: bool,
        name: Option<&str>,
        threshold: f64,
    ) -> Result<Vec<SyncToolsServerResult>>;

    async fn list_mappings(&self, name: Option<&str>) -> Result<Vec<ToolMappingData>>;

    /// List tools currently advertised by every registered MCP server
    /// (connects on demand). Mirrors `GET /api/mcp/tools`.
    async fn list_tools(&self) -> Result<Vec<McpToolMeta>>;

    /// Invoke a tool on a registered MCP server. Returns the typed MCP
    /// `tools/call` envelope. Mirrors `POST /api/mcp/run`.
    /// `arguments` is opaque — its shape is dictated by each upstream tool's own schema.
    #[allow(clippy::disallowed_types)]
    async fn run_tool(
        &self,
        server: &str,
        name: &str,
        arguments: Value,
    ) -> Result<crate::mgmt::RunMcpToolOutput>;
}

// ── Schema databases ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SchemaDbData {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: String,
    pub database: String,
    pub container: Option<String>,
    pub domains_file: Option<String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct SchemaDbInput {
    pub name: String,
    pub database: String,
    pub user: String,
    pub password: String,
    pub container: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub domains_file: Option<String>,
}

#[async_trait]
pub trait SchemaDbService: Send + Sync {
    async fn list(&self) -> Result<Vec<SchemaDbData>>;
    async fn upsert(&self, input: SchemaDbInput) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<bool>;

    /// Build the multi-tab schema view across every configured database.
    async fn schema(&self) -> Result<crate::mgmt::GetSchemaOutput>;

    /// Concatenate `domains` arrays from every configured database into a
    /// single flat list.
    async fn schema_domains(&self) -> Result<Vec<crate::mgmt::SchemaDomain>>;
}

// ── Docker runtimes ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DockerRuntimeData {
    pub name: String,
    pub socket_path: Option<String>,
    pub host: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct DockerRuntimeInput {
    pub name: String,
    pub socket_path: Option<String>,
    pub host: Option<String>,
    pub url: Option<String>,
}

#[async_trait]
pub trait DockerRuntimeService: Send + Sync {
    async fn list(&self) -> Result<Vec<DockerRuntimeData>>;
    async fn upsert(&self, input: DockerRuntimeInput) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<bool>;
}

// ── Doc roots + ignore patterns ─────────────────────────────────────────────

#[derive(Clone)]
pub struct DocRootData {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct DocRootInput {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
}

#[async_trait]
pub trait DocRootService: Send + Sync {
    async fn list_roots(&self) -> Result<Vec<DocRootData>>;
    async fn upsert_root(&self, input: DocRootInput) -> Result<()>;
    async fn remove_root(&self, name: &str) -> Result<bool>;

    async fn list_ignore_patterns(&self) -> Result<Vec<String>>;
    async fn add_ignore_pattern(&self, pattern: &str) -> Result<bool>;
    async fn remove_ignore_pattern(&self, pattern: &str) -> Result<bool>;
}

// ── Proxmox endpoints ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ProxmoxEndpointData {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub insecure: bool,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct ProxmoxEndpointInput {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub token_secret: String,
    pub insecure: bool,
}

#[async_trait]
pub trait ProxmoxEndpointService: Send + Sync {
    async fn list(&self) -> Result<Vec<ProxmoxEndpointData>>;
    async fn upsert(&self, input: ProxmoxEndpointInput) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<bool>;
}

// ── Home Assistant endpoints ────────────────────────────────────────────────

#[derive(Clone)]
pub struct HaEndpointData {
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct HaEndpointInput {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

#[async_trait]
pub trait HaEndpointService: Send + Sync {
    async fn list(&self) -> Result<Vec<HaEndpointData>>;
    async fn upsert(&self, input: HaEndpointInput) -> Result<()>;
    async fn remove(&self, name: &str) -> Result<bool>;
}

// ── Provide/register entry points (see `services::mod` doc) ─────────────

pub trait ProvideMcpRegistry {
    fn mcp_registry(&self) -> std::sync::Arc<dyn McpRegistryService>;
}
pub fn register_mcp_registry(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideMcpRegistry) {
    ctx.register_service(p.mcp_registry());
}

pub trait ProvideSchemaDb {
    fn schema_db(&self) -> std::sync::Arc<dyn SchemaDbService>;
}
pub fn register_schema_db(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideSchemaDb) {
    ctx.register_service(p.schema_db());
}

pub trait ProvideDockerRuntime {
    fn docker_runtime(&self) -> std::sync::Arc<dyn DockerRuntimeService>;
}
pub fn register_docker_runtime(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideDockerRuntime) {
    ctx.register_service(p.docker_runtime());
}

pub trait ProvideDocRoot {
    fn doc_root(&self) -> std::sync::Arc<dyn DocRootService>;
}
pub fn register_doc_root(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideDocRoot) {
    ctx.register_service(p.doc_root());
}

pub trait ProvideProxmoxEndpoint {
    fn proxmox_endpoint(&self) -> std::sync::Arc<dyn ProxmoxEndpointService>;
}
pub fn register_proxmox_endpoint(
    ctx: &mut orca_contract::ToolCtx,
    p: &impl ProvideProxmoxEndpoint,
) {
    ctx.register_service(p.proxmox_endpoint());
}

pub trait ProvideHaEndpoint {
    fn ha_endpoint(&self) -> std::sync::Arc<dyn HaEndpointService>;
}
pub fn register_ha_endpoint(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideHaEndpoint) {
    ctx.register_service(p.ha_endpoint());
}
