//! Management tools: MCP server registry, schema databases, Docker runtimes, doc roots.
use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;

use crate::mcp::handlers;

// ═══════════════════════════════════════════════════════════════════════════════
// MCP Server Management
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListMcpServersArgs {}
pub struct ListMcpServers;
#[async_trait]
impl OrcaTool for ListMcpServers {
    const NAME: &'static str = "list_mcp_servers";
    const DESCRIPTION: &'static str = "List all MCP servers registered in orca.db (orca's own managed registry). \
         Does not include ~/.claude.json servers managed by Claude Code directly.";
    type Args = ListMcpServersArgs;
    type Output = String;
    async fn run(_: ListMcpServersArgs, _: &ToolCtx) -> Result<String> {
        handlers::mcp_list_servers()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddMcpServerArgs {
    /// Server name (e.g. rebuy-cli)
    pub name: String,
    /// Executable command (e.g. node)
    pub command: String,
    /// Arguments
    pub args: Option<Vec<String>>,
    /// Environment variables as key/value pairs
    pub env: Option<HashMap<String, String>>,
}
pub struct AddMcpServer;
#[async_trait]
impl OrcaTool for AddMcpServer {
    const NAME: &'static str = "add_mcp_server";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Add or update an MCP server in orca.db. Use this when the user \
         wants to register a new MCP server for orca to federate.";
    type Args = AddMcpServerArgs;
    type Output = String;
    async fn run(args: AddMcpServerArgs, _: &ToolCtx) -> Result<String> {
        let row = db::mcp_servers::ServerRow {
            name: args.name.clone(),
            command: args.command,
            args: args.args.unwrap_or_default(),
            env: args.env.unwrap_or_default(),
            enabled: true,
        };
        let conn = db::open_default()?;
        db::mcp_servers::upsert(&conn, &row)?;
        Ok(format!("Registered MCP server '{}' in orca.db.", args.name))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveMcpServerArgs {
    /// Server name to remove
    pub name: String,
}
pub struct RemoveMcpServer;
#[async_trait]
impl OrcaTool for RemoveMcpServer {
    const NAME: &'static str = "remove_mcp_server";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove an MCP server from orca.db by name.";
    type Args = RemoveMcpServerArgs;
    type Output = String;
    async fn run(args: RemoveMcpServerArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::mcp_servers::remove(&conn, &args.name)? {
            Ok(format!("Removed MCP server '{}' from orca.db.", args.name))
        } else {
            Ok(format!("Server '{}' not found in orca.db.", args.name))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct MapToolArgs {
    /// Server name (must already be registered)
    pub name: String,
    pub orca_tool: String,
    pub external_tool: String,
}
pub struct MapTool;
#[async_trait]
impl OrcaTool for MapTool {
    const NAME: &'static str = "map_tool";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Map an orca tool name to a specific tool on a registered MCP server.";
    type Args = MapToolArgs;
    type Output = String;
    async fn run(args: MapToolArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::mcp_map_tool(&json!({
            "name": args.name,
            "orca_tool": args.orca_tool,
            "external_tool": args.external_tool
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct UnmapToolArgs {
    pub orca_tool: String,
}
pub struct UnmapTool;
#[async_trait]
impl OrcaTool for UnmapTool {
    const NAME: &'static str = "unmap_tool";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove a tool mapping from orca.db.";
    type Args = UnmapToolArgs;
    type Output = String;
    async fn run(args: UnmapToolArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::mcp_unmap_tool(&json!({ "orca_tool": args.orca_tool }))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SyncToolsArgs {
    /// Sync all registered servers
    pub all: Option<bool>,
    /// Sync a specific server by name
    pub name: Option<String>,
    /// Similarity threshold for auto-mapping (default 0.8)
    pub threshold: Option<f64>,
}
pub struct SyncTools;
#[async_trait]
impl OrcaTool for SyncTools {
    const NAME: &'static str = "sync_tools";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Auto-discover and map tools from registered MCP servers. \
         Provide name or set all=true.";
    type Args = SyncToolsArgs;
    type Output = String;
    async fn run(args: SyncToolsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::mcp_sync_tools(&json!({
            "all": args.all,
            "name": args.name,
            "threshold": args.threshold
        }))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ListToolMappingsArgs {
    /// Filter by server name (omit for all)
    pub name: Option<String>,
}
pub struct ListToolMappings;
#[async_trait]
impl OrcaTool for ListToolMappings {
    const NAME: &'static str = "list_tool_mappings";
    const DESCRIPTION: &'static str =
        "List all tool mappings in orca.db, optionally filtered by server name.";
    type Args = ListToolMappingsArgs;
    type Output = String;
    async fn run(args: ListToolMappingsArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::mcp_list_mappings(&json!({ "name": args.name }))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Schema Databases
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListSchemasArgs {}
pub struct ListSchemas;
#[async_trait]
impl OrcaTool for ListSchemas {
    const NAME: &'static str = "list_schemas";
    const DESCRIPTION: &'static str =
        "List all MySQL/MariaDB schema databases registered in orca.db.";
    type Args = ListSchemasArgs;
    type Output = String;
    async fn run(_: ListSchemasArgs, _: &ToolCtx) -> Result<String> {
        handlers::schema_list_databases()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddSchemaArgs {
    pub name: String,
    pub database: String,
    pub user: String,
    pub password: String,
    /// Docker container name (for docker exec connection)
    pub container: Option<String>,
    /// Host for direct TCP connection
    pub host: Option<String>,
    /// Port (default 3306)
    pub port: Option<u16>,
    /// Path to JSON domains file
    pub domains_file: Option<String>,
}
pub struct AddSchema;
#[async_trait]
impl OrcaTool for AddSchema {
    const NAME: &'static str = "add_schema";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Add or update a schema database in orca.db. \
         Use container OR host/port, not both.";
    type Args = AddSchemaArgs;
    type Output = String;
    async fn run(args: AddSchemaArgs, _: &ToolCtx) -> Result<String> {
        let row = db::schema_databases::SchemaDbRow {
            name: args.name.clone(),
            driver: "mysql".to_string(),
            host: args.host,
            port: args.port,
            user: args.user,
            password: args.password,
            database: args.database,
            container: args.container,
            domains_file: args.domains_file,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::schema_databases::upsert(&conn, &row)?;
        Ok(format!(
            "Registered schema database '{}' in orca.db.",
            args.name
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveSchemaArgs {
    pub name: String,
}
pub struct RemoveSchema;
#[async_trait]
impl OrcaTool for RemoveSchema {
    const NAME: &'static str = "remove_schema";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a schema database from orca.db by name.";
    type Args = RemoveSchemaArgs;
    type Output = String;
    async fn run(args: RemoveSchemaArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::schema_databases::remove(&conn, &args.name)? {
            Ok(format!(
                "Removed schema database '{}' from orca.db.",
                args.name
            ))
        } else {
            Ok(format!("Database '{}' not found in orca.db.", args.name))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Docker Runtimes
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListDockerRuntimesArgs {}
pub struct ListDockerRuntimes;
#[async_trait]
impl OrcaTool for ListDockerRuntimes {
    const NAME: &'static str = "list_docker_runtimes";
    const DESCRIPTION: &'static str = "List all Docker runtimes registered in orca.db.";
    type Args = ListDockerRuntimesArgs;
    type Output = String;
    async fn run(_: ListDockerRuntimesArgs, _: &ToolCtx) -> Result<String> {
        handlers::docker_list_runtimes()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddDockerRuntimeArgs {
    pub name: String,
    /// Path to Docker socket (e.g. /var/run/docker.sock)
    pub socket_path: Option<String>,
    /// Remote Docker host (e.g. ssh://user@host)
    pub host: Option<String>,
    /// Docker API URL
    pub url: Option<String>,
}
pub struct AddDockerRuntime;
#[async_trait]
impl OrcaTool for AddDockerRuntime {
    const NAME: &'static str = "add_docker_runtime";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register a Docker runtime in orca.db. \
         Provide socketPath, host, or url.";
    type Args = AddDockerRuntimeArgs;
    type Output = String;
    async fn run(args: AddDockerRuntimeArgs, _: &ToolCtx) -> Result<String> {
        if args.socket_path.is_none() && args.host.is_none() && args.url.is_none() {
            anyhow::bail!("provide socketPath, host, or url");
        }
        let row = db::docker_runtimes::RuntimeRow {
            name: args.name.clone(),
            socket_path: args.socket_path,
            host: args.host,
            url: args.url,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::docker_runtimes::upsert(&conn, &row)?;
        Ok(format!(
            "Registered Docker runtime '{}' in orca.db.",
            args.name
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveDockerRuntimeArgs {
    pub name: String,
}
pub struct RemoveDockerRuntime;
#[async_trait]
impl OrcaTool for RemoveDockerRuntime {
    const NAME: &'static str = "remove_docker_runtime";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Docker runtime from orca.db by name.";
    type Args = RemoveDockerRuntimeArgs;
    type Output = String;
    async fn run(args: RemoveDockerRuntimeArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::docker_runtimes::remove(&conn, &args.name)? {
            Ok(format!(
                "Removed Docker runtime '{}' from orca.db.",
                args.name
            ))
        } else {
            Ok(format!("Runtime '{}' not found in orca.db.", args.name))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Doc Root Registry
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListDocRootsArgs {}
pub struct ListDocRoots;
#[async_trait]
impl OrcaTool for ListDocRoots {
    const NAME: &'static str = "list_doc_roots";
    const DESCRIPTION: &'static str = "List all documentation roots registered in orca.db.";
    type Args = ListDocRootsArgs;
    type Output = String;
    async fn run(_: ListDocRootsArgs, _: &ToolCtx) -> Result<String> {
        handlers::doc_list_roots()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddDocRootArgs {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
}
pub struct AddDocRoot;
#[async_trait]
impl OrcaTool for AddDocRoot {
    const NAME: &'static str = "add_doc_root";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Register a documentation root directory in orca.db.";
    type Args = AddDocRootArgs;
    type Output = String;
    async fn run(args: AddDocRootArgs, _: &ToolCtx) -> Result<String> {
        let row = db::docs::RootRow {
            name: args.name.clone(),
            path: args.path.clone(),
            description: args.description,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::docs::upsert_root(&conn, &row)?;
        Ok(format!(
            "Registered doc root '{}' → {}",
            args.name, args.path
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveDocRootArgs {
    pub name: String,
}
pub struct RemoveDocRoot;
#[async_trait]
impl OrcaTool for RemoveDocRoot {
    const NAME: &'static str = "remove_doc_root";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a documentation root from orca.db by name.";
    type Args = RemoveDocRootArgs;
    type Output = String;
    async fn run(args: RemoveDocRootArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::docs::remove_root(&conn, &args.name)? {
            Ok(format!("Removed doc root '{}'.", args.name))
        } else {
            Ok(format!("Doc root '{}' not found.", args.name))
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ListDocIgnorePatternsArgs {}
pub struct ListDocIgnorePatterns;
#[async_trait]
impl OrcaTool for ListDocIgnorePatterns {
    const NAME: &'static str = "list_doc_ignore_patterns";
    const DESCRIPTION: &'static str =
        "List directory names excluded from all doc roots (e.g. node_modules, .git).";
    type Args = ListDocIgnorePatternsArgs;
    type Output = String;
    async fn run(_: ListDocIgnorePatternsArgs, _: &ToolCtx) -> Result<String> {
        handlers::doc_list_ignore_patterns()
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddDocIgnorePatternArgs {
    /// Pattern to ignore (e.g. node_modules)
    pub pattern: String,
}
pub struct AddDocIgnorePattern;
#[async_trait]
impl OrcaTool for AddDocIgnorePattern {
    const NAME: &'static str = "add_doc_ignore_pattern";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Add a directory name to the global doc ignore list.";
    type Args = AddDocIgnorePatternArgs;
    type Output = String;
    async fn run(args: AddDocIgnorePatternArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::doc_add_ignore_pattern(&json!({ "pattern": args.pattern }))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveDocIgnorePatternArgs {
    /// Pattern to remove
    pub pattern: String,
}
pub struct RemoveDocIgnorePattern;
#[async_trait]
impl OrcaTool for RemoveDocIgnorePattern {
    const NAME: &'static str = "remove_doc_ignore_pattern";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a directory name from the global doc ignore list.";
    type Args = RemoveDocIgnorePatternArgs;
    type Output = String;
    async fn run(args: RemoveDocIgnorePatternArgs, _: &ToolCtx) -> Result<String> {
        use serde_json::json;
        handlers::doc_remove_ignore_pattern(&json!({ "pattern": args.pattern }))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Proxmox Endpoints
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListProxmoxEndpointsArgs {}
pub struct ListProxmoxEndpoints;
#[async_trait]
impl OrcaTool for ListProxmoxEndpoints {
    const NAME: &'static str = "list_proxmox_endpoints";
    const DESCRIPTION: &'static str =
        "List all Proxmox VE endpoints registered in orca.db (token secrets are redacted).";
    type Args = ListProxmoxEndpointsArgs;
    type Output = String;
    async fn run(_: ListProxmoxEndpointsArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        let rows = db::proxmox::list(&conn)?;
        let redacted: Vec<_> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "base_url": r.base_url,
                    "token_id": r.token_id,
                    "token_secret": "***",
                    "insecure": r.insecure,
                    "enabled": r.enabled,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&redacted)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddProxmoxEndpointArgs {
    /// Logical name for this Proxmox endpoint (e.g. "thor/pve")
    pub name: String,
    /// Base URL including scheme + port (e.g. https://pve.lan:8006)
    pub base_url: String,
    /// Token ID in `user@realm!tokenid` format
    pub token_id: String,
    /// Token secret (UUID); stored as-is, surfaced only when this tool is called
    pub token_secret: String,
    /// Skip TLS verification — common for homelab self-signed certs
    pub insecure: Option<bool>,
}
pub struct AddProxmoxEndpoint;
#[async_trait]
impl OrcaTool for AddProxmoxEndpoint {
    const NAME: &'static str = "add_proxmox_endpoint";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register or update a Proxmox VE endpoint in orca.db. \
         Auth uses an API token (PVEAPIToken header).";
    type Args = AddProxmoxEndpointArgs;
    type Output = String;
    async fn run(args: AddProxmoxEndpointArgs, _: &ToolCtx) -> Result<String> {
        let row = db::proxmox::EndpointRow {
            name: args.name.clone(),
            base_url: args.base_url,
            token_id: args.token_id,
            token_secret: args.token_secret,
            insecure: args.insecure.unwrap_or(false),
            enabled: true,
        };
        let conn = db::open_default()?;
        db::proxmox::upsert(&conn, &row)?;
        Ok(format!(
            "Registered Proxmox endpoint '{}' in orca.db.",
            args.name
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveProxmoxEndpointArgs {
    pub name: String,
}
pub struct RemoveProxmoxEndpoint;
#[async_trait]
impl OrcaTool for RemoveProxmoxEndpoint {
    const NAME: &'static str = "remove_proxmox_endpoint";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Proxmox VE endpoint from orca.db by name.";
    type Args = RemoveProxmoxEndpointArgs;
    type Output = String;
    async fn run(args: RemoveProxmoxEndpointArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::proxmox::remove(&conn, &args.name)? {
            Ok(format!(
                "Removed Proxmox endpoint '{}' from orca.db.",
                args.name
            ))
        } else {
            Ok(format!("Endpoint '{}' not found in orca.db.", args.name))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Home Assistant Endpoints
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Deserialize, JsonSchema)]
pub struct ListHomeAssistantEndpointsArgs {}
pub struct ListHomeAssistantEndpoints;
#[async_trait]
impl OrcaTool for ListHomeAssistantEndpoints {
    const NAME: &'static str = "list_home_assistant_endpoints";
    const DESCRIPTION: &'static str =
        "List all Home Assistant endpoints registered in orca.db (tokens are redacted).";
    type Args = ListHomeAssistantEndpointsArgs;
    type Output = String;
    async fn run(_: ListHomeAssistantEndpointsArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        let rows = db::home_assistant::list(&conn)?;
        let redacted: Vec<_> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "base_url": r.base_url,
                    "token": "***",
                    "enabled": r.enabled,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&redacted)?)
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AddHomeAssistantEndpointArgs {
    /// Logical name for this Home Assistant endpoint (e.g. "home")
    pub name: String,
    /// Base URL including scheme + port (e.g. http://homeassistant.local:8123)
    pub base_url: String,
    /// Long-lived access token; stored as-is, surfaced only when this tool is called
    pub token: String,
}
pub struct AddHomeAssistantEndpoint;
#[async_trait]
impl OrcaTool for AddHomeAssistantEndpoint {
    const NAME: &'static str = "add_home_assistant_endpoint";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Register or update a Home Assistant endpoint in orca.db. \
         Auth uses a long-lived access token (Bearer header).";
    type Args = AddHomeAssistantEndpointArgs;
    type Output = String;
    async fn run(args: AddHomeAssistantEndpointArgs, _: &ToolCtx) -> Result<String> {
        let row = db::home_assistant::EndpointRow {
            name: args.name.clone(),
            base_url: args.base_url,
            token: args.token,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::home_assistant::upsert(&conn, &row)?;
        Ok(format!(
            "Registered Home Assistant endpoint '{}' in orca.db.",
            args.name
        ))
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RemoveHomeAssistantEndpointArgs {
    pub name: String,
}
pub struct RemoveHomeAssistantEndpoint;
#[async_trait]
impl OrcaTool for RemoveHomeAssistantEndpoint {
    const NAME: &'static str = "remove_home_assistant_endpoint";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a Home Assistant endpoint from orca.db by name.";
    type Args = RemoveHomeAssistantEndpointArgs;
    type Output = String;
    async fn run(args: RemoveHomeAssistantEndpointArgs, _: &ToolCtx) -> Result<String> {
        let conn = db::open_default()?;
        if db::home_assistant::remove(&conn, &args.name)? {
            Ok(format!(
                "Removed Home Assistant endpoint '{}' from orca.db.",
                args.name
            ))
        } else {
            Ok(format!("Endpoint '{}' not found in orca.db.", args.name))
        }
    }
}

// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    // MCP server management
    reg.register::<ListMcpServers>()
        .register::<AddMcpServer>()
        .register::<RemoveMcpServer>()
        .register::<MapTool>()
        .register::<UnmapTool>()
        .register::<SyncTools>()
        .register::<ListToolMappings>()
        // Schema databases
        .register::<ListSchemas>()
        .register::<AddSchema>()
        .register::<RemoveSchema>()
        // Docker runtimes
        .register::<ListDockerRuntimes>()
        .register::<AddDockerRuntime>()
        .register::<RemoveDockerRuntime>()
        // Proxmox endpoints
        .register::<ListProxmoxEndpoints>()
        .register::<AddProxmoxEndpoint>()
        .register::<RemoveProxmoxEndpoint>()
        // Home Assistant endpoints
        .register::<ListHomeAssistantEndpoints>()
        .register::<AddHomeAssistantEndpoint>()
        .register::<RemoveHomeAssistantEndpoint>()
        // Doc root registry
        .register::<ListDocRoots>()
        .register::<AddDocRoot>()
        .register::<RemoveDocRoot>()
        .register::<ListDocIgnorePatterns>()
        .register::<AddDocIgnorePattern>()
        .register::<RemoveDocIgnorePattern>();
}
