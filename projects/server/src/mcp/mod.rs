#![allow(clippy::disallowed_types)] // MCP JSON-RPC protocol — opaque tool args/results required
/// MCP stdio server — exposes orca tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: orca mcp-serve
/// Register: claude mcp add orca-local -- orca mcp-serve
// Server-side tool-implementations moved to `crate::services::*` — only
// the MCP-protocol pieces (handlers, context7 federation, run_agent legacy
// static tool defs) stay here.
mod tools;

use anyhow::Result;
use contract::ToolCtx;
use contract::config::Config;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub fn build_tool_ctx(config: Arc<Config>) -> ToolCtx {
    let mut ctx = ToolCtx::new(config);
    // Host-addressing refresh hook: host.refresh tool calls into this to
    // trigger a fresh detect + persist before reading host_addressing rows.
    let host_refresh: Arc<dyn system::host::HostRefreshHook + Send + Sync> =
        Arc::new(system::host_identity::ServerHostRefreshHook);
    ctx.register_service(host_refresh);
    // Peer transport for `cli::exec_remote` (orca-dispatch dispatches
    // remote_ok tools through whatever RemoteExec the host registers).
    let remote: Arc<dyn contract::RemoteExec> = Arc::new(pod::PodRemoteExec);
    ctx.register_service(remote);
    // Pod-host cluster roster — plugin-agnostic discovery used by
    // `pod.snapshot` so the systems UI can group peers by cluster without
    // depending on a specific virtualization plugin. The installed service is
    // an aggregator that fans out across every roster provider registered in
    // `contract::cluster_roster` — contributed by a loaded cdylib plugin
    // (proxmox, …) through the loader's `cluster_roster` domain.
    let cluster_roster: Arc<dyn contract::ClusterRoster> =
        Arc::new(contract::cluster_roster::AggregateClusterRoster);
    ctx.register_service(cluster_roster);
    dispatch::remote_ok::install(dispatch::remote_ok_names());
    dispatch::tool_roles::install(dispatch::role_table());
    match resolve_host_operator() {
        Some(id) => ctx.with_auth(id),
        None => ctx,
    }
}

/// Resolve the host's ambient operator identity for minting signed caller
/// tokens on the CLI/MCP remote-dispatch path. Reads the on-disk session
/// written by `orca auth login` (see [[project-orca-login-local-auth]]),
/// validates it against `sessions`, and slides expiry by the CLI TTL.
/// Returns `None` when there is no active session — remote admin tools then
/// refuse with the recipient's normal zero-trust handling. No `first_admin`
/// fallback: local DB access does not imply admin.
fn resolve_host_operator() -> Option<contract::CallerIdentity> {
    let path = files::ops::orca_home()?.join("session");
    let sid = std::fs::read_to_string(&path).ok()?;
    let sid = sid.trim();
    if sid.is_empty() {
        return None;
    }
    let conn = db::open_default().ok()?;
    let row = db::sessions::find_active(&conn, sid).ok().flatten()?;
    let now = chrono::Utc::now();
    let exp_parsed = chrono::DateTime::parse_from_rfc3339(&row.expires_at).ok()?;
    if exp_parsed <= now {
        return None;
    }
    let new_exp = now + chrono::Duration::seconds(::auth::auth::CLI_SESSION_TTL_SECS);
    db::sessions::touch(&conn, sid, &now.to_rfc3339(), &new_exp.to_rfc3339()).ok();
    Some(contract::CallerIdentity {
        user_id: row.user_id,
        username: row.username,
        role: row.role,
    })
}

pub async fn serve(config: &Config) -> Result<()> {
    // Reqwest is built with `rustls-no-provider`; without this the first HTTPS
    // client construction panics with "No provider set". Mirrors `build_router`.
    ::model::ensure_crypto_provider();

    let config_arc = Arc::new(config.clone());
    let tool_ctx = build_tool_ctx(config_arc);

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
                let registry_defs = dispatch::mcp_definitions();
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

                // Federated MCP-server tools are contributed by the `mcp`
                // plugin: they are declared as plugin tools (loaded above via
                // `load_plugin_tool_rows`) and dispatched through the plugin
                // bridge, so there is no separate federation passthrough here.
                reply(id, json!({ "tools": all_orca }))
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
                } else if dispatch::names().contains(&name) {
                    // MCP wants text — Value::String passes through, structs pretty-print.
                    let result = dispatch::dispatch_text(name, args.clone(), &tool_ctx).await;
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
                    reply(
                        id,
                        json!({ "content": [{ "type": "text", "text": format!("Error: unknown tool: {name}") }], "isError": true }),
                    )
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
    let token = auth::loopback_token::get()
        .map(|s| s.to_string())
        .or_else(auth::loopback_token::read_from_disk)
        .context("loopback token unavailable — is the daemon running?")?;
    let resp = auth::loopback_token::loopback_only_reqwest_client(&url)?
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
