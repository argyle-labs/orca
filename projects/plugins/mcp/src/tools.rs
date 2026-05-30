//! `system.mcp.*` and `system.mcp.federation.*` orca_tools. Direct calls to
//! `db::mcp_servers` / `db::tool_mappings` for the registry CRUD; federation
//! tools use [`crate::client::McpPool`].
#![allow(clippy::disallowed_types)] // MCP `arguments` blob — opaque per the MCP spec

use derive::orca_tool;
use serde_json::Value;
use std::sync::Arc;

use crate::client::McpPool;
use crate::sync::mcp_sync_server;
use crate::types::{
    AddMcpServerArgs, ListMcpServersArgs, ListMcpServersOutput, ListMcpToolsArgs,
    ListMcpToolsOutput, ListToolMappingsArgs, ListToolMappingsOutput, MapToolArgs, MapToolResult,
    MappingEntry, McpContent, McpServerEntry, McpServerMutationResult, McpToolEntry,
    RemoveMcpServerArgs, RunMcpToolArgs, RunMcpToolOutput, SyncToolsArgs, SyncToolsOutput,
    SyncToolsServerEntry, UnmapToolArgs, UnmapToolResult,
};

/// Build an `McpPool` rooted at orca's default DB path.
fn make_mcp_pool() -> McpPool {
    use contract::config::{APP_DB_FILE, APP_STATE_DIR};
    if let Ok(path) = std::env::var("ORCA_DB_PATH") {
        return McpPool::new_with_db(std::path::PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        return McpPool::new_with_db(home.join(APP_STATE_DIR).join(APP_DB_FILE));
    }
    McpPool::new()
}

// ── MCP registry CRUD ───────────────────────────────────────────────────────

/// List all MCP servers registered in orca.db (orca's own managed registry). Does not include ~/.claude.json servers managed by Claude Code directly.
#[orca_tool(domain = "system.mcp", verb = "list")]
async fn list_mcp_servers(
    _args: ListMcpServersArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListMcpServersOutput> {
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?
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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<McpServerMutationResult> {
    let row = db::mcp_servers::ServerRow {
        name: args.name.clone(),
        command: args.command,
        args: args.args.unwrap_or_default(),
        env: args.env.unwrap_or_default(),
        enabled: true,
    };
    let conn = db::open_default()?;
    db::mcp_servers::upsert(&conn, &row)?;
    Ok(McpServerMutationResult {
        name: args.name,
        changed: true,
    })
}

/// [MUTATES STATE] Remove an MCP server from orca.db by name.
#[orca_tool(domain = "system.mcp", verb = "delete")]
async fn remove_mcp_server(
    args: RemoveMcpServerArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<McpServerMutationResult> {
    let conn = db::open_default()?;
    let changed = db::mcp_servers::remove(&conn, &args.name)?;
    Ok(McpServerMutationResult {
        name: args.name,
        changed,
    })
}

/// [MUTATES STATE] Map an orca tool name to a specific tool on a registered MCP server.
#[orca_tool(domain = "system.mcp.mapping", verb = "create")]
async fn mcp_mapping_create(
    args: MapToolArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MapToolResult> {
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?;
    if !servers.iter().any(|s| s.name == args.name) {
        anyhow::bail!(
            "MCP server '{}' not found — register it first with add_mcp_server",
            args.name
        );
    }
    let row = db::tool_mappings::MappingRow {
        orca_tool: args.orca_tool.clone(),
        mcp_name: args.name.clone(),
        external_tool: args.external_tool.clone(),
        match_type: "explicit".to_string(),
        confidence: None,
        enabled: true,
    };
    db::tool_mappings::upsert(&conn, &row)?;
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
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<UnmapToolResult> {
    let conn = db::open_default()?;
    let changed = db::tool_mappings::remove(&conn, &args.orca_tool)?;
    Ok(UnmapToolResult {
        orca_tool: args.orca_tool,
        changed,
    })
}

/// [MUTATES STATE] Auto-discover and map tools from registered MCP servers. Provide name or set all=true.
#[orca_tool(domain = "system.mcp", verb = "sync")]
async fn sync_tools(
    args: SyncToolsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SyncToolsOutput> {
    let threshold = args.threshold.unwrap_or(0.8);
    let all = args.all.unwrap_or(false);
    let name = args.name.as_deref();
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
    let mut results = Vec::new();
    for s in targets {
        match mcp_sync_server(s, threshold) {
            Ok((added, skipped)) => results.push(SyncToolsServerEntry {
                server: s.name.clone(),
                added: added as u32,
                skipped: skipped as u32,
                error: None,
            }),
            Err(e) => results.push(SyncToolsServerEntry {
                server: s.name.clone(),
                added: 0,
                skipped: 0,
                error: Some(e.to_string()),
            }),
        }
    }
    Ok(SyncToolsOutput { results })
}

/// List all tool mappings in orca.db, optionally filtered by server name.
#[orca_tool(domain = "system.mcp.mapping", verb = "list")]
async fn mcp_mapping_list(
    args: ListToolMappingsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListToolMappingsOutput> {
    let conn = db::open_default()?;
    let rows = if let Some(n) = args.name.as_deref() {
        db::tool_mappings::list(&conn, n)?
    } else {
        db::tool_mappings::all(&conn)?
    };
    let mappings = rows
        .into_iter()
        .map(|r| MappingEntry {
            orca_tool: r.orca_tool,
            mcp_name: r.mcp_name,
            external_tool: r.external_tool,
            match_type: r.match_type,
            confidence: r.confidence,
            enabled: r.enabled,
        })
        .collect();
    Ok(ListToolMappingsOutput { mappings })
}

// ── MCP federation passthrough ──────────────────────────────────────────────

/// List every tool advertised by every registered MCP server (connects on demand).
#[orca_tool(domain = "system.mcp.federation", verb = "list-tools")]
async fn list_mcp_tools(
    _args: ListMcpToolsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListMcpToolsOutput> {
    let pool = make_mcp_pool();
    let raw = pool.all_tools().await;
    let tools = raw
        .into_iter()
        .map(|v| McpToolEntry {
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
            input_schema: v
                .get("inputSchema")
                .cloned()
                .and_then(|x| serde_json::from_value(x).ok())
                .unwrap_or_default(),
        })
        .collect();
    Ok(ListMcpToolsOutput { tools })
}

/// [MUTATES STATE] Invoke a tool on a registered MCP server. Returns the typed `tools/call` envelope (`{ content, isError, structuredContent? }`).
#[orca_tool(domain = "system.mcp.federation", verb = "run", cli = skip)]
async fn run_mcp_tool(
    args: RunMcpToolArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RunMcpToolOutput> {
    let arguments = match args.args {
        Some(m) => Value::Object(m),
        None => serde_json::json!({}),
    };
    let pool = Arc::new(make_mcp_pool());
    let client = pool
        .get_or_connect(&args.server)
        .await
        .map_err(|e| anyhow::anyhow!("connect to mcp server '{}': {e}", args.server))?;
    let cid = "tool:run_mcp_tool";
    let raw = match client.call_tool(&args.tool, arguments, cid).await {
        Ok(v) => v,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("MCP server closed") {
                pool.evict(&args.server).await;
            }
            return Err(e);
        }
    };
    Ok(parse_mcp_call_result(raw))
}

/// Parse the JSON-RPC `result` value of an MCP `tools/call` response into a
/// typed envelope. Tolerant of partially-shaped servers: missing fields fall
/// back to sensible defaults rather than failing the call.
fn parse_mcp_call_result(raw: Value) -> RunMcpToolOutput {
    let obj = match raw {
        Value::Object(m) => m,
        other => {
            return RunMcpToolOutput {
                content: vec![McpContent {
                    kind: "text".to_string(),
                    text: Some(other.to_string()),
                    data: None,
                    mime_type: None,
                    resource: None,
                }],
                is_error: false,
                structured_content: None,
            };
        }
    };

    let is_error = obj
        .get("isError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let structured_content = obj.get("structuredContent").cloned();

    let content = match obj.get("content") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                serde_json::from_value::<McpContent>(item.clone()).unwrap_or_else(|_| McpContent {
                    kind: item
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    text: item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    data: item
                        .get("data")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    mime_type: item
                        .get("mimeType")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    resource: item.get("resource").cloned(),
                })
            })
            .collect(),
        _ => Vec::new(),
    };

    RunMcpToolOutput {
        content,
        is_error,
        structured_content,
    }
}
