//! Service traits for the `mgmt` domain — split into ~6 sub-services per
//! sub-domain (MCP servers + tool mappings, schema DBs, Docker runtimes,
//! doc roots + ignore patterns, Proxmox endpoints, Home Assistant endpoints).

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

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
    pub input_schema: Value,
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

    /// Invoke a tool on a registered MCP server. Returns the opaque tool
    /// result `Value`. Mirrors `POST /api/mcp/run`.
    async fn run_tool(&self, server: &str, name: &str, arguments: Value) -> Result<Value>;
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
    /// Returns an opaque JSON object `{ tabs, showTabs, errors? }` — see
    /// `SchemaResponse` in the server crate for the full shape.
    async fn schema(&self) -> Result<Value>;

    /// Concatenate `domains` arrays from every configured database into a
    /// single flat JSON array.
    async fn schema_domains(&self) -> Result<Value>;
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
