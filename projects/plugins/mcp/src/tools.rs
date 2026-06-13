//! MCP tool surface — flat surface (`mcp.{list, detail, update, delete, run}`).
//! An MCP server is the resource; its tool mappings nest into the row.
//! `update` covers register / map / unmap / sync — args determine which.
//!
//! `mcp.run` envelopes opaque MCP `tools/call` payloads — that JSON shape is
//! upstream-defined by the MCP spec so the typed surface stops at the
//! envelope.
#![allow(clippy::disallowed_types)]

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json as sj;
use std::collections::HashMap;
use std::sync::Arc;

use crate::client::McpPool;
use crate::sync::mcp_sync_server;
use crate::types::{
    MappingEntry, McpContent, McpServerEntry, McpToolEntry, SyncToolsOutput, SyncToolsServerEntry,
};
use utils::json_schema::JsonSchemaNode;

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

// ═══════════════════════════════════════════════════════════════════════════
// mcp.list — every registered MCP server with mappings nested
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpServerRow {
    #[serde(flatten)]
    pub server: McpServerEntry,
    pub mappings: Vec<MappingEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct McpListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpListOutput {
    pub servers: Vec<McpServerRow>,
}

/// List every registered MCP server with its tool mappings nested.
#[orca_tool(domain = "mcp", verb = "list")]
async fn mcp_list(_args: McpListArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<McpListOutput> {
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?;
    let all_mappings = db::tool_mappings::all(&conn)?;
    let rows = servers
        .into_iter()
        .map(|s| {
            let mappings = all_mappings
                .iter()
                .filter(|m| m.mcp_name == s.name)
                .map(|m| MappingEntry {
                    orca_tool: m.orca_tool.clone(),
                    mcp_name: m.mcp_name.clone(),
                    external_tool: m.external_tool.clone(),
                    match_type: m.match_type.clone(),
                    confidence: m.confidence,
                    enabled: m.enabled,
                })
                .collect();
            McpServerRow {
                server: McpServerEntry {
                    name: s.name,
                    command: s.command,
                    args: s.args,
                    env: s.env,
                    enabled: s.enabled,
                },
                mappings,
            }
        })
        .collect();
    Ok(McpListOutput { servers: rows })
}

// ═══════════════════════════════════════════════════════════════════════════
// mcp.detail — one server + mappings + live tool advertisement
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct McpDetailArgs {
    /// Server name. Omit to return the full federated tool catalogue across all servers.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpDetailOutput {
    /// Populated when `name` was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<McpServerRow>,
    /// Live tool advertisement. When `name` is set, only that server's tools;
    /// when omitted, every registered server's tools.
    pub tools: Vec<McpToolEntry>,
}

#[orca_tool(domain = "mcp", verb = "detail")]
async fn mcp_detail(
    args: McpDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<McpDetailOutput> {
    let pool = make_mcp_pool();
    let raw_tools = pool.all_tools().await;

    let filter_name = args.name.as_deref();
    let tools: Vec<McpToolEntry> = raw_tools
        .into_iter()
        .filter_map(|v| {
            let server = v.get("server").and_then(|s| s.as_str())?.to_string();
            if let Some(n) = filter_name
                && server != n
            {
                return None;
            }
            Some(McpToolEntry {
                server,
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
                    .and_then(|x| sj::from_value::<JsonSchemaNode>(x).ok())
                    .unwrap_or_default(),
            })
        })
        .collect();

    let server = if let Some(name) = filter_name {
        let conn = db::open_default()?;
        let servers = db::mcp_servers::list(&conn)?;
        let s = servers
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("server '{name}' not found"))?;
        let mappings = db::tool_mappings::list(&conn, name)?
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
        Some(McpServerRow {
            server: McpServerEntry {
                name: s.name,
                command: s.command,
                args: s.args,
                env: s.env,
                enabled: s.enabled,
            },
            mappings,
        })
    } else {
        None
    };

    Ok(McpDetailOutput { server, tools })
}

// ═══════════════════════════════════════════════════════════════════════════
// mcp.update — register/update a server, map/unmap a tool, or sync
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpUpdateArgs {
    /// Server name. Required for register/map/unmap/sync (unless `sync_all=true`).
    #[arg(long)]
    pub name: Option<String>,
    /// Register-or-update: when set, upserts the server row using `name`+this+args+env.
    #[arg(long)]
    pub command: Option<String>,
    /// Arg list for the server command.
    #[arg(long)]
    pub args: Option<Vec<String>>,
    /// Env map (REST/MCP only — CLI K=V parsing not currently supported).
    #[arg(skip)]
    pub env: Option<HashMap<String, String>>,

    /// Tool mapping: set `map_orca_tool` + `map_external_tool` (uses `name` as the server).
    #[arg(long)]
    pub map_orca_tool: Option<String>,
    #[arg(long)]
    pub map_external_tool: Option<String>,
    /// Remove a tool mapping by orca tool name.
    #[arg(long)]
    pub unmap_orca_tool: Option<String>,

    /// Auto-discover and map tools. Requires either `name` or `sync_all=true`.
    #[arg(long)]
    pub sync: bool,
    /// When set with `sync`, runs against every registered server.
    #[arg(long)]
    pub sync_all: bool,
    /// Fuzzy-match threshold for `sync` (default 0.8).
    #[arg(long)]
    pub sync_threshold: Option<f64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpUpdateOutput {
    /// Notes per applied sub-operation.
    pub applied: Vec<String>,
    /// Populated when `sync` ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncToolsOutput>,
}

/// [MUTATES STATE] Register/update a server, add or remove a tool mapping,
/// and/or run a tool sync. Multiple sub-operations can be combined.
#[orca_tool(domain = "mcp", verb = "update")]
async fn mcp_update(
    args: McpUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<McpUpdateOutput> {
    let mut out = McpUpdateOutput::default();
    let conn = db::open_default()?;

    if let Some(command) = &args.command {
        let name = args
            .name
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("name required to register a server"))?;
        let row = db::mcp_servers::ServerRow {
            name: name.to_string(),
            command: command.clone(),
            args: args.args.clone().unwrap_or_default(),
            env: args.env.clone().unwrap_or_default(),
            enabled: true,
        };
        db::mcp_servers::upsert(&conn, &row)?;
        out.applied.push(format!("server-upserted:{name}"));
    }

    match (
        args.map_orca_tool.as_deref(),
        args.map_external_tool.as_deref(),
    ) {
        (Some(orca), Some(ext)) => {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("name required to create a mapping"))?;
            let servers = db::mcp_servers::list(&conn)?;
            if !servers.iter().any(|s| s.name == name) {
                anyhow::bail!("MCP server '{name}' not registered");
            }
            db::tool_mappings::upsert(
                &conn,
                &db::tool_mappings::MappingRow {
                    orca_tool: orca.to_string(),
                    mcp_name: name.to_string(),
                    external_tool: ext.to_string(),
                    match_type: "explicit".to_string(),
                    confidence: None,
                    enabled: true,
                },
            )?;
            out.applied.push(format!("mapping:{orca}->{name}:{ext}"));
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("map_orca_tool and map_external_tool must be set together");
        }
        (None, None) => {}
    }

    if let Some(orca) = &args.unmap_orca_tool {
        let changed = db::tool_mappings::remove(&conn, orca)?;
        out.applied.push(format!(
            "unmapped:{orca}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }

    if args.sync {
        let threshold = args.sync_threshold.unwrap_or(0.8);
        let servers = db::mcp_servers::list(&conn)?;
        let targets: Vec<&db::mcp_servers::ServerRow> = if args.sync_all {
            servers.iter().collect()
        } else {
            let name = args
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("sync requires name or sync_all=true"))?;
            vec![
                servers
                    .iter()
                    .find(|s| s.name == name)
                    .ok_or_else(|| anyhow::anyhow!("server '{name}' not found"))?,
            ]
        };
        let results = targets
            .into_iter()
            .map(|s| match mcp_sync_server(s, threshold) {
                Ok((added, skipped)) => SyncToolsServerEntry {
                    server: s.name.clone(),
                    added: added as u32,
                    skipped: skipped as u32,
                    error: None,
                },
                Err(e) => SyncToolsServerEntry {
                    server: s.name.clone(),
                    added: 0,
                    skipped: 0,
                    error: Some(e.to_string()),
                },
            })
            .collect();
        out.sync = Some(SyncToolsOutput { results });
        out.applied.push("sync".to_string());
    }

    if out.applied.is_empty() {
        anyhow::bail!("no update operation specified");
    }
    Ok(out)
}

// ═══════════════════════════════════════════════════════════════════════════
// mcp.delete — remove a server (cascades mappings)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct McpDeleteArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct McpDeleteOutput {
    pub name: String,
    pub changed: bool,
}

#[orca_tool(domain = "mcp", verb = "delete")]
async fn mcp_delete(
    args: McpDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<McpDeleteOutput> {
    let conn = db::open_default()?;
    let changed = db::mcp_servers::remove(&conn, &args.name)?;
    Ok(McpDeleteOutput {
        name: args.name,
        changed,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// mcp.run — execute a tool on a registered MCP server (RPC verb)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct McpRunArgs {
    pub server: String,
    pub tool: String,
    /// Opaque MCP `tools/call` arguments — upstream-defined per the MCP spec.
    #[serde(default)]
    #[arg(skip)]
    pub args: Option<sj::Map<String, sj::Value>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpRunOutput {
    pub content: Vec<McpContent>,
    pub is_error: bool,
    /// Opaque structured payload — upstream-defined per the MCP spec.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<sj::Value>,
}

/// [MUTATES STATE] Invoke a tool on a registered MCP server. Returns the typed
/// `tools/call` envelope.
#[orca_tool(domain = "mcp", verb = "run", cli = skip)]
async fn mcp_run(args: McpRunArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<McpRunOutput> {
    let arguments = match args.args {
        Some(m) => sj::Value::Object(m),
        None => sj::json!({}),
    };
    let pool = Arc::new(make_mcp_pool());
    let client = pool
        .get_or_connect(&args.server)
        .await
        .map_err(|e| anyhow::anyhow!("connect to mcp server '{}': {e}", args.server))?;
    let cid = "tool:mcp.run";
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

fn parse_mcp_call_result(raw: sj::Value) -> McpRunOutput {
    let obj = match raw {
        sj::Value::Object(m) => m,
        other => {
            return McpRunOutput {
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
        Some(sj::Value::Array(items)) => items
            .iter()
            .map(|item| {
                sj::from_value::<McpContent>(item.clone()).unwrap_or_else(|_| McpContent {
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

    McpRunOutput {
        content,
        is_error,
        structured_content,
    }
}
