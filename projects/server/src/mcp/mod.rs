#![allow(clippy::disallowed_types)] // MCP JSON-RPC protocol — opaque tool args/results required
/// MCP stdio server — exposes orca tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: orca mcp-serve
/// Register: claude mcp add orca-local -- orca mcp-serve
pub mod agent_resolve;
pub mod agents_service;
pub mod auth_service;
mod context7;
pub mod db_admin_service;
pub mod docker_service;
pub mod docs;
pub mod docs_service;
mod handlers;
pub mod infra_service;
pub mod lifecycle_service;
pub mod mgmt_service;
pub mod pki_service;
pub mod plugin_runtime_service;
pub mod plugins_service;
pub mod profile_service;
pub mod secrets_service;
pub mod spec_registry_service;
mod spec_tools;
mod specs;
pub mod system_service;
mod tools;

use anyhow::Result;
use orca_utils::config::Config;
use orca_utils::tool::{ToolCtx, ToolRegistry};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use handlers::run;

/// Populate a ToolRegistry with every server-side OrcaTool. Single source of
/// truth — called by MCP stdio, the HTTP /api/tools router, and (eventually)
/// the WASM client surface.
pub fn register_all_tools(reg: &mut ToolRegistry) {
    // First-party tools that live in the neutral `orca-tools` crate. The
    // `orca_tools!` macro there is the single enrollment point; new tools
    // should land in `projects/tools/` and join that macro.
    orca_tools::register_all(reg);

    // Server-coupled tools that still live here pending service-trait
    // abstractions so they can move to projects/tools/ too.
    spec_tools::register(reg);
}

pub fn build_tool_registry(config: Arc<Config>) -> (ToolRegistry, ToolCtx) {
    use orca_tools_def::services::agent_backend::AgentBackendService;
    let mut ctx = ToolCtx::new(config);
    let agent_backend: Arc<dyn AgentBackendService> =
        Arc::new(crate::llm::agent_backend_service::ServerAgentBackend);
    ctx.register_service(agent_backend);
    let agents_svc: Arc<dyn orca_tools_def::services::agents::AgentsService> =
        Arc::new(crate::mcp::agents_service::ServerAgents {
            config: ctx.config.clone(),
        });
    ctx.register_service(agents_svc);
    let docs_svc: Arc<dyn orca_tools_def::services::docs::DocsService> =
        Arc::new(crate::mcp::docs_service::ServerDocs {
            config: ctx.config.clone(),
        });
    ctx.register_service(docs_svc);
    let infra_svc: Arc<dyn orca_tools_def::services::infra::InfraService> =
        Arc::new(crate::mcp::infra_service::ServerInfra);
    ctx.register_service(infra_svc);
    let plugins_svc: Arc<dyn orca_tools_def::services::plugins::PluginsService> =
        Arc::new(crate::mcp::plugins_service::ServerPlugins);
    ctx.register_service(plugins_svc);
    let plugin_runtime: Arc<dyn orca_tools_def::services::plugin_runtime::PluginRuntimeService> =
        Arc::new(crate::mcp::plugin_runtime_service::ServerPluginRuntime);
    ctx.register_service(plugin_runtime);
    let docker_svc: Arc<dyn orca_tools_def::services::docker::DockerService> =
        Arc::new(crate::mcp::docker_service::ServerDocker);
    ctx.register_service(docker_svc);
    let spec_registry: Arc<dyn orca_tools_def::services::spec_registry::SpecRegistryService> =
        Arc::new(crate::mcp::spec_registry_service::ServerSpecRegistry);
    ctx.register_service(spec_registry);
    let system_svc: Arc<dyn orca_tools_def::services::system::SystemService> =
        Arc::new(crate::mcp::system_service::ServerSystem);
    ctx.register_service(system_svc);
    let auth_svc: Arc<dyn orca_tools_def::services::auth::AuthService> =
        Arc::new(crate::mcp::auth_service::ServerAuth);
    ctx.register_service(auth_svc);
    let db_admin: Arc<dyn orca_tools_def::services::db_admin::DbAdminService> =
        Arc::new(crate::mcp::db_admin_service::ServerDbAdmin);
    ctx.register_service(db_admin);
    let pki_svc: Arc<dyn orca_tools_def::services::pki::PkiService> =
        Arc::new(crate::mcp::pki_service::ServerPki);
    ctx.register_service(pki_svc);
    let profile_svc: Arc<dyn orca_tools_def::services::profile::ProfileService> =
        Arc::new(crate::mcp::profile_service::ServerProfile {
            config: ctx.config.clone(),
        });
    ctx.register_service(profile_svc);
    let secrets_svc: Arc<dyn orca_tools_def::services::secrets::SecretsService> =
        Arc::new(crate::mcp::secrets_service::DbSecretsService::new());
    ctx.register_service(secrets_svc.clone());
    // Host-addressing refresh hook: host.refresh tool calls into this to
    // trigger a fresh detect + persist before reading host_addressing rows.
    let host_refresh: Arc<dyn orca_tools_def::host::HostRefreshHook + Send + Sync> =
        Arc::new(crate::host_identity::ServerHostRefreshHook);
    ctx.register_service(host_refresh);
    let lifecycle_svc: Arc<dyn orca_tools_def::services::lifecycle::LifecycleService> =
        Arc::new(crate::mcp::lifecycle_service::ServerLifecycle {
            config: ctx.config.clone(),
            secrets: secrets_svc,
        });
    ctx.register_service(lifecycle_svc);
    {
        use orca_tools_def::services::mgmt::*;
        let mcp_reg: Arc<dyn McpRegistryService> =
            Arc::new(crate::mcp::mgmt_service::ServerMcpRegistry);
        ctx.register_service(mcp_reg);
        let schemas: Arc<dyn SchemaDbService> = Arc::new(crate::mcp::mgmt_service::ServerSchemaDb);
        ctx.register_service(schemas);
        let docker_rt: Arc<dyn DockerRuntimeService> =
            Arc::new(crate::mcp::mgmt_service::ServerDockerRuntime);
        ctx.register_service(docker_rt);
        let doc_root: Arc<dyn DocRootService> = Arc::new(crate::mcp::mgmt_service::ServerDocRoot);
        ctx.register_service(doc_root);
        let proxmox_ep: Arc<dyn ProxmoxEndpointService> =
            Arc::new(crate::mcp::mgmt_service::ServerProxmoxEndpoint);
        ctx.register_service(proxmox_ep);
        let ha_ep: Arc<dyn HaEndpointService> =
            Arc::new(crate::mcp::mgmt_service::ServerHaEndpoint);
        ctx.register_service(ha_ep);
    }
    let mut reg = ToolRegistry::new();
    register_all_tools(&mut reg);
    (reg, ctx)
}

/// Servers whose tools orca already exposes natively or that must not be proxied back.
/// - orca-local: orca itself — proxying would spawn a recursive child
const FEDERATION_SKIP: &[&str] = &["orca-local"];

pub async fn serve(config: &Config) -> Result<()> {
    // Reqwest is built with `rustls-no-provider`; without this the first HTTPS
    // client construction (e.g. on tools/list federation calls) panics with
    // "No provider set" and Claude Code sees zero tools. Mirrors `build_router`.
    crate::llm::ensure_crypto_provider();

    let pool = crate::serve::mcp_client::McpPool::new_with_db(config.db_path.clone());

    let config_arc = Arc::new(config.clone());
    let (orca_registry, tool_ctx) = build_tool_registry(config_arc);

    // Maps exposed tool name → (server_name, internal_tool_name).
    // For universal-mapped tools: exposed name differs from internal name.
    // For pass-through tools: both names are the same.
    let mut tool_registry: HashMap<String, (String, String)> = HashMap::new();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut out = tokio::io::BufWriter::new(stdout);

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // MCP notifications (no id) are fire-and-forget — replying would break the protocol.
        if req.get("id").is_none() {
            continue;
        }

        let response = match method {
            "initialize" => reply(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "orca", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" => reply(id, json!({})),
            "tools/list" => {
                // Registry-derived tools replace the corresponding static entries in tools.rs.
                // During migration: registry names shadow the static list.
                let registry_defs = orca_registry.mcp_definitions();
                let registry_names: std::collections::HashSet<String> = registry_defs
                    .iter()
                    .filter_map(|t| t["name"].as_str().map(str::to_string))
                    .collect();

                let static_tools = tools::tool_defs();
                let mut all_orca: Vec<Value> = registry_defs;
                // Include static tools not yet migrated to the registry
                if let Some(arr) = static_tools.as_array() {
                    for t in arr {
                        if t["name"]
                            .as_str()
                            .is_none_or(|n| !registry_names.contains(n as &str))
                        {
                            all_orca.push(t.clone());
                        }
                    }
                }

                // Plugin-declared tools (`<plugin_id>.<tool>`) registered via
                // orca/tools.declare. Pulled from orca.db so this stdio child
                // sees them without a shared in-process registry. The actual
                // dispatch is forwarded to the daemon's HTTP API.
                for row in load_plugin_tool_rows() {
                    let schema: Value = serde_json::from_str(&row.input_schema)
                        .unwrap_or_else(|_| json!({"type": "object"}));
                    all_orca.push(json!({
                        "name": row.fq_name,
                        "description": row.description,
                        "inputSchema": schema,
                    }));
                }

                let orca_names: std::collections::HashSet<&str> =
                    all_orca.iter().filter_map(|t| t["name"].as_str()).collect();

                // Discover tools from federated servers, skipping orca-local
                let external = pool.all_tools_filtered(FEDERATION_SKIP).await;

                tool_registry.clear();
                for tool in &external {
                    let name = tool["name"].as_str().unwrap_or("");
                    let server = tool["server"].as_str().unwrap_or("");
                    let alias = tool["alias"].as_str().unwrap_or(name);
                    if !name.is_empty() && !server.is_empty() && !orca_names.contains(name) {
                        tool_registry
                            .insert(name.to_string(), (server.to_string(), alias.to_string()));
                    }
                }

                let mut all_tools = all_orca;
                for mut tool in external {
                    let name = tool["name"].as_str().unwrap_or("").to_string();
                    if tool_registry.contains_key(&name) {
                        if let Some(obj) = tool.as_object_mut() {
                            obj.remove("server");
                            obj.remove("alias");
                        }
                        all_tools.push(tool);
                    }
                }

                reply(id, json!({ "tools": all_tools }))
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];

                if name.contains('.') && is_plugin_tool(name) {
                    // Plugin-declared tool. Forward to the daemon, which
                    // dispatches via the in-process PluginRegistry.
                    match call_plugin_tool(name, args).await {
                        Ok(result) => reply(
                            id,
                            json!({
                                "content": [{
                                    "type": "text",
                                    "text": serde_json::to_string(&result).unwrap_or_default()
                                }],
                                "isError": false,
                                "structuredContent": result,
                            }),
                        ),
                        Err(e) => reply(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": format!("Error: {e}") }],
                                "isError": true
                            }),
                        ),
                    }
                } else if let Some((server_name, internal_name)) = tool_registry.get(name).cloned()
                {
                    // Route to the owning federated server using the internal tool name
                    match pool.get_or_connect(&server_name).await {
                        Err(e) => reply(
                            id,
                            json!({
                                "content": [{ "type": "text", "text": format!("Error connecting to {server_name}: {e}") }],
                                "isError": true
                            }),
                        ),
                        Ok(client) => {
                            let cid = id.to_string();
                            match client.call_tool(&internal_name, args.clone(), &cid).await {
                                Ok(result) => reply(id, result),
                                Err(e) => {
                                    let msg = e.to_string();
                                    if msg.contains("MCP server closed") {
                                        pool.evict(&server_name).await;
                                    }
                                    reply(
                                        id,
                                        json!({
                                            "content": [{ "type": "text", "text": format!("Error: {msg}") }],
                                            "isError": true
                                        }),
                                    )
                                }
                            }
                        }
                    }
                } else if orca_registry.names().contains(&name) {
                    // MCP wants text — Value::String passes through, structs pretty-print.
                    let result = orca_registry
                        .dispatch_text(name, args.clone(), &tool_ctx)
                        .await;
                    match result {
                        Ok(text) => reply(
                            id,
                            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                        ),
                        Err(e) => reply(
                            id,
                            json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true }),
                        ),
                    }
                } else {
                    // Legacy dispatch for tools not yet migrated to OrcaTool
                    let result = dispatch(name, args, config).await;
                    match result {
                        Ok(text) => reply(
                            id,
                            json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                        ),
                        Err(e) => reply(
                            id,
                            json!({ "content": [{ "type": "text", "text": format!("Error: {e}") }], "isError": true }),
                        ),
                    }
                }
            }
            _ => error_reply(id, -32601, &format!("method not found: {method}")),
        };

        let mut payload = serde_json::to_string(&response)?;
        payload.push('\n');
        out.write_all(payload.as_bytes()).await?;
        out.flush().await?;
    }

    Ok(())
}

// ── Plugin tool bridge ────────────────────────────────────────────────────────
//
// `mcp-serve` is a stdio child process spawned by Claude — distinct from the
// orca daemon, so it cannot share the in-process `PluginRegistry`. Plugin tool
// declarations are read from orca.db (cheap, no IPC); calls are forwarded to
// the daemon's HTTP endpoint, which dispatches via the registry.

const PLUGIN_TOOL_HTTP: &str = "http://127.0.0.1:12000";
const PLUGIN_TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);

fn load_plugin_tool_rows() -> Vec<db::plugin_tools::PluginToolRow> {
    match db::open_default().and_then(|c| db::plugin_tools::list_all(&c)) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("[mcp] could not load plugin tools from db: {e}");
            Vec::new()
        }
    }
}

fn is_plugin_tool(fq_name: &str) -> bool {
    db::open_default()
        .and_then(|c| db::plugin_tools::get(&c, fq_name))
        .map(|r| r.is_some())
        .unwrap_or(false)
}

async fn call_plugin_tool(fq_name: &str, args: &Value) -> Result<Value> {
    use anyhow::Context;
    let url = format!("{PLUGIN_TOOL_HTTP}/api/plugin-tools/{fq_name}/call");
    let body = json!({ "arguments": args.clone() });
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .timeout(PLUGIN_TOOL_CALL_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .context("plugin tool response was not JSON")?;
    if !status.is_success() {
        let msg = payload
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error");
        anyhow::bail!(
            "plugin tool '{fq_name}' failed ({}): {msg}",
            status.as_u16()
        );
    }
    Ok(payload
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

// Legacy dispatch — only tools not yet converted to OrcaTool remain here.
// TODO: convert run_agent, then delete this function entirely.
async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        "run_agent" => run(args, config).await,
        "resolve_library" | "get_library_docs" => {
            context7::proxy_context7(name, args, config).await
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
