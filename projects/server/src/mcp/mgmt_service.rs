//! Server-side impls of the 6 mgmt sub-services. Each is a thin shim over a
//! `db::*` sub-module; sync_tools delegates to `commands::mcp_sync_server`.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::services::mgmt::{
    DocRootData, DocRootInput, DocRootService, DockerRuntimeData, DockerRuntimeInput,
    DockerRuntimeService, HaEndpointData, HaEndpointInput, HaEndpointService, McpRegistryService,
    McpServerData, McpServerInput, McpToolMeta, ProxmoxEndpointData, ProxmoxEndpointInput,
    ProxmoxEndpointService, SchemaDbData, SchemaDbInput, SchemaDbService, SyncToolsServerResult,
    ToolMappingData,
};
use serde_json::Value;

/// Build an `McpPool` rooted at orca's default DB path. Used by the
/// federation-aware service methods (`list_tools`, `run_tool`) so they see
/// the same set of registered servers as the HTTP `/api/mcp/*` handlers.
fn make_mcp_pool() -> crate::serve::mcp_client::McpPool {
    use orca_utils::config::{APP_DB_FILE, APP_STATE_DIR};
    if let Ok(path) = std::env::var("ORCA_DB_PATH") {
        return crate::serve::mcp_client::McpPool::new_with_db(std::path::PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        return crate::serve::mcp_client::McpPool::new_with_db(
            home.join(APP_STATE_DIR).join(APP_DB_FILE),
        );
    }
    crate::serve::mcp_client::McpPool::new()
}

// ── MCP registry ────────────────────────────────────────────────────────────

pub struct ServerMcpRegistry;

#[async_trait]
impl McpRegistryService for ServerMcpRegistry {
    async fn list_servers(&self) -> Result<Vec<McpServerData>> {
        let conn = db::open_default()?;
        Ok(db::mcp_servers::list(&conn)?
            .into_iter()
            .map(|s| McpServerData {
                name: s.name,
                command: s.command,
                args: s.args,
                env: s.env,
                enabled: s.enabled,
            })
            .collect())
    }

    async fn upsert_server(&self, input: McpServerInput) -> Result<()> {
        let row = db::mcp_servers::ServerRow {
            name: input.name,
            command: input.command,
            args: input.args,
            env: input.env,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::mcp_servers::upsert(&conn, &row)
    }

    async fn remove_server(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::mcp_servers::remove(&conn, name)
    }

    async fn map_tool(&self, name: &str, orca_tool: &str, external_tool: &str) -> Result<()> {
        let conn = db::open_default()?;
        let servers = db::mcp_servers::list(&conn)?;
        if !servers.iter().any(|s| s.name == name) {
            anyhow::bail!("MCP server '{name}' not found — register it first with add_mcp_server");
        }
        let row = db::tool_mappings::MappingRow {
            orca_tool: orca_tool.to_string(),
            mcp_name: name.to_string(),
            external_tool: external_tool.to_string(),
            match_type: "explicit".to_string(),
            confidence: None,
            enabled: true,
        };
        db::tool_mappings::upsert(&conn, &row)
    }

    async fn unmap_tool(&self, orca_tool: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::tool_mappings::remove(&conn, orca_tool)
    }

    async fn sync_tools(
        &self,
        all: bool,
        name: Option<&str>,
        threshold: f64,
    ) -> Result<Vec<SyncToolsServerResult>> {
        if !all && name.is_none() {
            anyhow::bail!("provide name or set all=true");
        }
        let conn = db::open_default()?;
        let servers = db::mcp_servers::list(&conn)?;
        let targets: Vec<&db::mcp_servers::ServerRow> = if all {
            servers.iter().collect()
        } else {
            let n = name.expect("checked above");
            vec![
                servers
                    .iter()
                    .find(|s| s.name == n)
                    .ok_or_else(|| anyhow::anyhow!("server '{n}' not found"))?,
            ]
        };
        let mut out = Vec::new();
        for s in targets {
            match crate::commands::mcp_sync_server(s, threshold) {
                Ok((added, skipped)) => out.push(SyncToolsServerResult {
                    server: s.name.clone(),
                    added: added as u32,
                    skipped: skipped as u32,
                    error: None,
                }),
                Err(e) => out.push(SyncToolsServerResult {
                    server: s.name.clone(),
                    added: 0,
                    skipped: 0,
                    error: Some(e.to_string()),
                }),
            }
        }
        Ok(out)
    }

    async fn list_tools(&self) -> Result<Vec<McpToolMeta>> {
        let pool = make_mcp_pool();
        let raw = pool.all_tools().await;
        Ok(raw
            .into_iter()
            .map(|v| McpToolMeta {
                server: v
                    .get("server")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: v
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: v
                    .get("description")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: v.get("inputSchema").cloned().unwrap_or(Value::Null),
            })
            .collect())
    }

    async fn run_tool(&self, server: &str, name: &str, arguments: Value) -> Result<Value> {
        let pool = make_mcp_pool();
        let client = pool
            .get_or_connect(server)
            .await
            .map_err(|e| anyhow::anyhow!("connect to mcp server '{server}': {e}"))?;
        // Stable correlation id — this trait method is invoked outside the
        // HTTP middleware chain.
        let cid = "tool:run_mcp_tool";
        match client.call_tool(name, arguments, cid).await {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("MCP server closed") {
                    pool.evict(server).await;
                }
                Err(e)
            }
        }
    }

    async fn list_mappings(&self, name: Option<&str>) -> Result<Vec<ToolMappingData>> {
        let conn = db::open_default()?;
        let rows = if let Some(n) = name {
            db::tool_mappings::list(&conn, n)?
        } else {
            db::tool_mappings::all(&conn)?
        };
        Ok(rows
            .into_iter()
            .map(|r| ToolMappingData {
                orca_tool: r.orca_tool,
                mcp_name: r.mcp_name,
                external_tool: r.external_tool,
                match_type: r.match_type,
                confidence: r.confidence,
                enabled: r.enabled,
            })
            .collect())
    }
}

// ── Schema databases ────────────────────────────────────────────────────────

pub struct ServerSchemaDb;

#[async_trait]
impl SchemaDbService for ServerSchemaDb {
    async fn list(&self) -> Result<Vec<SchemaDbData>> {
        let conn = db::open_default()?;
        Ok(db::schema_databases::list(&conn)?
            .into_iter()
            .map(|d| SchemaDbData {
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
            .collect())
    }

    async fn upsert(&self, input: SchemaDbInput) -> Result<()> {
        let row = db::schema_databases::SchemaDbRow {
            name: input.name,
            driver: "mysql".to_string(),
            host: input.host,
            port: input.port,
            user: input.user,
            password: input.password,
            database: input.database,
            container: input.container,
            domains_file: input.domains_file,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::schema_databases::upsert(&conn, &row)
    }

    async fn remove(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::schema_databases::remove(&conn, name)
    }

    async fn schema(&self) -> Result<Value> {
        crate::serve::api::schema::build_schema_response()
            .await
            .map_err(|(status, msg)| anyhow::anyhow!("[{}] {msg}", status.as_u16()))
    }

    async fn schema_domains(&self) -> Result<Value> {
        Ok(crate::serve::api::schema::build_schema_domains())
    }
}

// ── Docker runtimes ─────────────────────────────────────────────────────────

pub struct ServerDockerRuntime;

#[async_trait]
impl DockerRuntimeService for ServerDockerRuntime {
    async fn list(&self) -> Result<Vec<DockerRuntimeData>> {
        let conn = db::open_default()?;
        Ok(db::docker_runtimes::list(&conn)?
            .into_iter()
            .map(|r| DockerRuntimeData {
                name: r.name,
                socket_path: r.socket_path,
                host: r.host,
                url: r.url,
                enabled: r.enabled,
            })
            .collect())
    }

    async fn upsert(&self, input: DockerRuntimeInput) -> Result<()> {
        if input.socket_path.is_none() && input.host.is_none() && input.url.is_none() {
            anyhow::bail!("provide socket_path, host, or url");
        }
        let row = db::docker_runtimes::RuntimeRow {
            name: input.name,
            socket_path: input.socket_path,
            host: input.host,
            url: input.url,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::docker_runtimes::upsert(&conn, &row)
    }

    async fn remove(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::docker_runtimes::remove(&conn, name)
    }
}

// ── Doc roots + ignore patterns ─────────────────────────────────────────────

pub struct ServerDocRoot;

#[async_trait]
impl DocRootService for ServerDocRoot {
    async fn list_roots(&self) -> Result<Vec<DocRootData>> {
        let conn = db::open_default()?;
        Ok(db::docs::list_roots(&conn)?
            .into_iter()
            .map(|r| DocRootData {
                name: r.name,
                path: r.path,
                description: r.description,
                enabled: r.enabled,
            })
            .collect())
    }

    async fn upsert_root(&self, input: DocRootInput) -> Result<()> {
        let row = db::docs::RootRow {
            name: input.name,
            path: input.path,
            description: input.description,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::docs::upsert_root(&conn, &row)
    }

    async fn remove_root(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::docs::remove_root(&conn, name)
    }

    async fn list_ignore_patterns(&self) -> Result<Vec<String>> {
        let conn = db::open_default()?;
        db::docs::list_ignore_patterns(&conn)
    }

    async fn add_ignore_pattern(&self, pattern: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::docs::add_ignore_pattern(&conn, pattern)
    }

    async fn remove_ignore_pattern(&self, pattern: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::docs::remove_ignore_pattern(&conn, pattern)
    }
}

// ── Proxmox endpoints ───────────────────────────────────────────────────────

pub struct ServerProxmoxEndpoint;

#[async_trait]
impl ProxmoxEndpointService for ServerProxmoxEndpoint {
    async fn list(&self) -> Result<Vec<ProxmoxEndpointData>> {
        let conn = db::open_default()?;
        Ok(db::proxmox::list(&conn)?
            .into_iter()
            .map(|r| ProxmoxEndpointData {
                name: r.name,
                base_url: r.base_url,
                token_id: r.token_id,
                insecure: r.insecure,
                enabled: r.enabled,
            })
            .collect())
    }

    async fn upsert(&self, input: ProxmoxEndpointInput) -> Result<()> {
        let row = db::proxmox::EndpointRow {
            name: input.name,
            base_url: input.base_url,
            token_id: input.token_id,
            token_secret: input.token_secret,
            insecure: input.insecure,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::proxmox::upsert(&conn, &row)
    }

    async fn remove(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::proxmox::remove(&conn, name)
    }
}

// ── Home Assistant endpoints ────────────────────────────────────────────────

pub struct ServerHaEndpoint;

#[async_trait]
impl HaEndpointService for ServerHaEndpoint {
    async fn list(&self) -> Result<Vec<HaEndpointData>> {
        let conn = db::open_default()?;
        Ok(db::home_assistant::list(&conn)?
            .into_iter()
            .map(|r| HaEndpointData {
                name: r.name,
                base_url: r.base_url,
                enabled: r.enabled,
            })
            .collect())
    }

    async fn upsert(&self, input: HaEndpointInput) -> Result<()> {
        let row = db::home_assistant::EndpointRow {
            name: input.name,
            base_url: input.base_url,
            token: input.token,
            enabled: true,
        };
        let conn = db::open_default()?;
        db::home_assistant::upsert(&conn, &row)
    }

    async fn remove(&self, name: &str) -> Result<bool> {
        let conn = db::open_default()?;
        db::home_assistant::remove(&conn, name)
    }
}
