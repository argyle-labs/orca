#![allow(clippy::disallowed_types)] // MCP JSON-RPC protocol — opaque tool args/results required
/// MCP stdio server — exposes orca tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: orca mcp-serve
/// Register: claude mcp add orca-local -- orca mcp-serve
// Server-side tool-implementations moved to `crate::services::*` — only
// the MCP-protocol pieces (handlers, context7 federation, run_agent legacy
// static tool defs) stay here.
mod context7;
pub mod docs;
mod handlers;
mod spec_tools;
mod specs;
mod tools;

use anyhow::Result;
use orca_contract::ToolCtx;
use orca_utils::config::Config;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use handlers::run;

/// Concrete embedder that satisfies the per-service `Provide*` traits in
/// each domain crate (`agents::*`, `fleet::*`, `platform::*`, ...). Each
/// `impl ProvideFoo for ServerEmbedder` is the single source of truth for
/// which server-side type backs that service.
struct ServerEmbedder {
    config: Arc<Config>,
}

impl agents::agent_backend::ProvideAgentBackend for ServerEmbedder {
    fn agent_backend(&self) -> Arc<dyn agents::agent_backend::AgentBackendService> {
        Arc::new(crate::llm::agent_backend_service::ServerAgentBackend)
    }
}

impl agents::agents::ProvideAgents for ServerEmbedder {
    fn agents(&self) -> Arc<dyn agents::agents::AgentsService> {
        Arc::new(crate::services::agents::ServerAgents {
            config: self.config.clone(),
        })
    }
}

pub fn build_tool_ctx(config: Arc<Config>) -> ToolCtx {
    let embedder = ServerEmbedder {
        config: config.clone(),
    };
    let mut ctx = ToolCtx::new(config);
    agents::agent_backend::register_agent_backend(&mut ctx, &embedder);
    agents::agents::register_agents(&mut ctx, &embedder);
    let docs_svc: Arc<dyn ::docs::docs::DocsService> =
        Arc::new(crate::services::docs::ServerDocs {
            config: ctx.config.clone(),
        });
    ctx.register_service(docs_svc);
let spec_registry: Arc<dyn ::docs::spec_registry::SpecRegistryService> =
        Arc::new(crate::services::spec_registry::ServerSpecRegistry);
    ctx.register_service(spec_registry);
    let system_svc: Arc<dyn fleet::system::SystemService> =
        Arc::new(crate::services::system::ServerSystem);
    ctx.register_service(system_svc);
    let profile_svc: Arc<dyn platform::profile::ProfileService> =
        Arc::new(crate::services::profile::ServerProfile {
            config: ctx.config.clone(),
        });
    ctx.register_service(profile_svc);
    // Host-addressing refresh hook: host.refresh tool calls into this to
    // trigger a fresh detect + persist before reading host_addressing rows.
    let host_refresh: Arc<dyn fleet::host::HostRefreshHook + Send + Sync> =
        Arc::new(crate::host_identity::ServerHostRefreshHook);
    ctx.register_service(host_refresh);
    let pod_svc: Arc<dyn fleet::pod::PodService> = Arc::new(crate::services::pod::ServerPod);
    ctx.register_service(pod_svc);
    let lifecycle_svc: Arc<dyn fleet::lifecycle::LifecycleService> =
        Arc::new(crate::services::lifecycle::ServerLifecycle {
            config: ctx.config.clone(),
        });
    ctx.register_service(lifecycle_svc);
    {
        use mgmt::mgmt::*;
        let mcp_reg: Arc<dyn McpRegistryService> =
            Arc::new(crate::services::mgmt::ServerMcpRegistry);
        ctx.register_service(mcp_reg);
        let schemas: Arc<dyn SchemaDbService> = Arc::new(crate::services::mgmt::ServerSchemaDb);
        ctx.register_service(schemas);
        let docker_rt: Arc<dyn DockerRuntimeService> =
            Arc::new(crate::services::mgmt::ServerDockerRuntime);
        ctx.register_service(docker_rt);
        let doc_root: Arc<dyn DocRootService> = Arc::new(crate::services::mgmt::ServerDocRoot);
        ctx.register_service(doc_root);
        let proxmox_ep: Arc<dyn ProxmoxEndpointService> =
            Arc::new(crate::services::mgmt::ServerProxmoxEndpoint);
        ctx.register_service(proxmox_ep);
        let ha_ep: Arc<dyn HaEndpointService> = Arc::new(crate::services::mgmt::ServerHaEndpoint);
        ctx.register_service(ha_ep);
    }
    crate::remote_ok::install(orca_dispatch::remote_ok_names());
    crate::tool_roles::install(orca_dispatch::role_table());
    ctx
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
    let tool_ctx = build_tool_ctx(config_arc);

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
                let registry_defs = orca_dispatch::mcp_definitions();
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
                } else if orca_dispatch::names().contains(&name) {
                    // MCP wants text — Value::String passes through, structs pretty-print.
                    let result = orca_dispatch::dispatch_text(name, args.clone(), &tool_ctx).await;
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

const PLUGIN_TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);

/// Loopback base URL for plugin-tool HTTP dispatch. Read fresh from env
/// so an operator override (`ORCA_HTTPS_PORT=…`) takes effect without
/// recompiling. Cheap (pure env parse).
fn plugin_tool_http_base() -> String {
    let ports = db::ports::current();
    format!("https://127.0.0.1:{}", ports.https)
}

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
    let url = format!(
        "{}/api/plugin-tools/{fq_name}/call",
        plugin_tool_http_base()
    );
    let body = json!({ "arguments": args.clone() });
    // Loopback HTTPS to the same-process daemon: self-signed core-CA cert,
    // accept invalid so we don't have to thread the CA root through here.
    let token = crate::loopback_token::get()
        .map(|s| s.to_string())
        .or_else(crate::loopback_token::read_from_disk)
        .context("loopback token unavailable — is the daemon running?")?;
    let resp = crate::loopback_token::loopback_only_reqwest_client(&url)?
        .post(&url)
        .bearer_auth(token)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_has_expected_shape() {
        let v = reply(json!(1), json!({"ok": true}));
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn error_reply_has_expected_shape() {
        let v = error_reply(json!("x"), -32601, "method not found");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "x");
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "method not found");
    }

    #[test]
    fn plugin_tool_http_is_loopback() {
        // The dispatch base must stay loopback regardless of which port the
        // operator overrode — it's the same-process daemon, not a peer.
        let base = plugin_tool_http_base();
        assert!(
            base.contains("127.0.0.1") || base.contains("localhost") || base.contains("[::1]"),
            "plugin_tool_http_base must target loopback: {base}"
        );
    }
}
