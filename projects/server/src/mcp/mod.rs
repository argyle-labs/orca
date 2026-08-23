#![allow(clippy::disallowed_types)] // MCP JSON-RPC protocol — opaque tool args/results required
/// MCP stdio server — exposes orca tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: orca mcp-serve
/// Register: claude mcp add orca-local -- orca mcp-serve
// Server-side tool-implementations moved to `crate::services::*` — only
// the MCP-protocol pieces (handlers, context7 federation, run_agent legacy
// static tool defs) stay here.
mod tools;
use ::mcp::context7;

use anyhow::Result;
use contract::ToolCtx;
use contract::config::Config;
use serde_json::{Value, json};
use std::collections::HashMap;
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
    // Everything is REMOTE_OK by default; install only the `local_only`
    // opt-outs as the denylist (reachability is default-allow, tightened per
    // tool via `local_only`, and separately gated by role).
    dispatch::remote_ok::install(dispatch::local_only_names());
    // Fold in the fixed diagnostics surface ops (not OrcaToolDefs): make
    // `diagnostics.repair` an admin + data-mutation op so delegated,
    // runtime-mutating repairs are gated like any other write.
    dispatch::tool_roles::install(
        dispatch::role_table()
            .into_iter()
            .chain(dispatch::diagnostics_surface::diagnostics_role_pairs())
            .chain(dispatch::ups_surface::ups_role_pairs()),
    );
    dispatch::tool_roles::install_mutations(
        dispatch::data_mutation_names()
            .into_iter()
            .chain(dispatch::diagnostics_surface::diagnostics_mutation_names())
            .chain(dispatch::ups_surface::ups_mutation_names()),
    );
    match resolve_host_operator().or_else(resolve_token_operator) {
        Some(id) => ctx.with_auth(id),
        None => ctx,
    }
}

/// Fallback operator identity for `mcp-serve` when there is no interactive
/// session on disk: present the admin bearer token supplied via `ORCA_TOKEN`
/// (or `ORCA_MCP_TOKEN`), resolved through the SAME `api_tokens` → replicated
/// `users` path as REST bearer auth. This is an explicit credential — the
/// operator must supply a valid admin token — so it does NOT violate the
/// "local DB access does not imply admin" rule that bars a `first_admin`
/// fallback. Without it, remote peer-dispatch over MCP cannot mint a signed
/// CallerToken and every admin mesh op refuses (see the mesh-admin-auth gap).
fn resolve_token_operator() -> Option<contract::CallerIdentity> {
    let token = std::env::var("ORCA_TOKEN")
        .ok()
        .or_else(|| std::env::var("ORCA_MCP_TOKEN").ok())?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    crate::serve::middleware::caller_from_token(token)
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
    let row = auth::sessions::find_active(&conn, sid).ok().flatten()?;
    let now = utils::time::now();
    let exp_parsed = utils::time::Timestamp::parse_rfc3339(&row.expires_at).ok()?;
    if exp_parsed <= now {
        return None;
    }
    let new_exp = now.plus(std::time::Duration::from_secs(
        ::auth::auth::CLI_SESSION_TTL_SECS as u64,
    ));
    auth::sessions::touch(&conn, sid, &now.to_rfc3339(), &new_exp.to_rfc3339()).ok();
    Some(contract::CallerIdentity {
        user_id: row.user_id,
        username: row.username,
        role: row.role,
    })
}

/// Servers whose tools orca already exposes natively or that must not be proxied back.
/// - orca-local: orca itself — proxying would spawn a recursive child
const FEDERATION_SKIP: &[&str] = &["orca-local"];

pub async fn serve(config: &Config) -> Result<()> {
    // Reqwest is built with `rustls-no-provider`; without this the first HTTPS
    // client construction (e.g. on tools/list federation calls) panics with
    // "No provider set" and Claude Code sees zero tools. Mirrors `build_router`.
    ::model::ensure_crypto_provider();

    let pool = ::mcp::client::McpPool::new_with_db(config.db_path.clone());

    let config_arc = Arc::new(config.clone());
    let tool_ctx = build_tool_ctx(config_arc);

    // Maps exposed tool name → (server_name, internal_tool_name).
    // For universal-mapped tools: exposed name differs from internal name.
    // For pass-through tools: both names are the same.
    let mut tool_registry: HashMap<String, (String, String)> = HashMap::new();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    // Shared with the catalog-watcher task below, which pushes
    // `notifications/tools/list_changed` on the same stream — guard it so a
    // notification never interleaves a response mid-write.
    let out = Arc::new(tokio::sync::Mutex::new(tokio::io::BufWriter::new(stdout)));

    // Auto-refresh without a manual reconnect: watch the live daemon catalog and,
    // when it changes (a self-update added/removed tools), emit
    // `notifications/tools/list_changed`. The client re-requests tools/list, which
    // #136 already serves from the daemon — so new tools appear on their own.
    // Gated on `initialized` so we never notify before the handshake.
    let initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let watch_out = Arc::clone(&out);
        let watch_init = Arc::clone(&initialized);
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            // Seed the baseline so we only fire on a CHANGE after startup.
            let mut last_sig = catalog_signature().await;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                if !watch_init.load(Ordering::Relaxed) {
                    continue;
                }
                let sig = catalog_signature().await;
                // Only notify on a real change (ignore transient daemon-down Nones).
                if sig.is_some() && sig != last_sig {
                    last_sig = sig;
                    let mut note =
                        json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" })
                            .to_string();
                    note.push('\n');
                    let mut o = watch_out.lock().await;
                    // Best-effort: if the client pipe is gone the next response
                    // write will surface it; a dropped notification is harmless.
                    o.write_all(note.as_bytes()).await.ok();
                    o.flush().await.ok();
                }
            }
        });
    }

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
            "initialize" => {
                // Report the LIVE daemon's version, not this child's compiled
                // one — a long-lived bridge outlives self-updates, so its own
                // CARGO_PKG_VERSION goes stale. Fall back to compiled-in only
                // when the daemon is unreachable.
                let version = daemon_version()
                    .await
                    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
                // Arm the catalog watcher: after the handshake it may push
                // tools/list_changed. Advertise listChanged so the client honors it.
                initialized.store(true, std::sync::atomic::Ordering::Relaxed);
                reply(
                    id,
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": "orca", "version": version }
                    }),
                )
            }
            "ping" => reply(id, json!({})),
            "tools/list" => {
                // Prefer the LIVE daemon's catalog so a long-lived bridge that
                // outlived a self-update projects the daemon's current tool
                // surface, not its own frozen inventory. Fall back to the
                // compiled-in catalog only when the daemon is unreachable.
                let all_orca: Vec<Value> = match fetch_daemon_catalog().await {
                    Some((_version, tools)) => tools,
                    None => core_tool_catalog(),
                };

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
                } else if dispatch::names().contains(&name) {
                    // Ambient-input overlay — the MCP equivalent of REST's
                    // header extraction in `http_dispatch`. JSON-RPC has no
                    // header/flag channel for a tool call, so peer-dispatch and
                    // correlation-id ride as reserved keys inside `arguments`.
                    // Strip them and fold onto a per-call ctx clone (base ctx
                    // stays immutable across concurrent calls) so the universal
                    // macro peer-dispatch stanza fires for every remote_ok tool.
                    let (clean_args, peer, correlation_id) = dispatch::take_ambient(args.clone());
                    let ctx_owned = if peer.is_some() || correlation_id.is_some() {
                        let mut ctx = tool_ctx.clone();
                        ctx.set_peer(peer);
                        ctx.set_correlation_id(correlation_id);
                        Some(ctx)
                    } else {
                        None
                    };
                    let ctx_ref: &ToolCtx = ctx_owned.as_ref().unwrap_or(&tool_ctx);
                    // MCP wants text — Value::String passes through, structs pretty-print.
                    let result = dispatch::dispatch_text(name, clean_args, ctx_ref).await;
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
                    // Not in THIS (possibly stale) child's in-process registry.
                    // The live daemon may have gained this tool since the bridge
                    // launched — forward there first. Only if the daemon also
                    // doesn't know it do we fall through to legacy federation
                    // dispatch (context7).
                    match call_core_tool_via_daemon(name, args).await {
                        Ok(Some(result)) => reply(
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
                        Ok(None) => {
                            // Legacy dispatch for tools not on the registry at all.
                            match dispatch(name, args, config).await {
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
        {
            let mut o = out.lock().await;
            o.write_all(payload.as_bytes()).await?;
            o.flush().await?;
        }
    }

    Ok(())
}

/// A cheap fingerprint of the live daemon catalog: `(version, hash-of-tool-names)`.
/// The watcher compares successive signatures to decide whether to push a
/// `tools/list_changed`. `None` when the daemon is unreachable (treated as "no
/// change" so a transient outage doesn't spam notifications).
async fn catalog_signature() -> Option<(String, u64)> {
    let (version, tools) = fetch_daemon_catalog().await?;
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    Some((version, tool_names_hash(&names)))
}

/// Order-independent hash of a tool-name set. Two catalogs with the same names
/// in any order hash equal; adding/removing a tool changes the hash.
fn tool_names_hash(names: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for n in &sorted {
        n.hash(&mut h);
    }
    h.finish()
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

/// The orca-core tool catalog (registry-derived + not-yet-migrated static +
/// plugin-declared), WITHOUT federated servers. Shared by two callers:
///   1. the daemon's `/api/mcp/catalog` handler (fresh binary — authoritative);
///   2. this stdio child's `tools/list`, as the fallback when the daemon is
///      unreachable.
///
/// This is the seam that un-pins the MCP bridge: a long-lived `mcp-serve`
/// child normally serves ITS OWN compiled inventory, so tools added by a
/// self-update never appear until the child is restarted. `tools/list` now
/// prefers the daemon's copy of this list (via `fetch_daemon_catalog`), so a
/// stale child projects the live daemon's surface instead of its frozen one.
pub fn core_tool_catalog() -> Vec<Value> {
    let registry_defs = dispatch::mcp_definitions();
    let registry_names: std::collections::HashSet<String> = registry_defs
        .iter()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect();

    let mut all_orca: Vec<Value> = registry_defs;
    // Static tools not yet migrated to the registry.
    if let Some(arr) = tools::tool_defs().as_array() {
        for t in arr {
            if t["name"]
                .as_str()
                .is_none_or(|n| !registry_names.contains(n as &str))
            {
                all_orca.push(t.clone());
            }
        }
    }
    // Plugin-declared tools (`<plugin_id>.<tool>`) from orca.db.
    for row in load_plugin_tool_rows() {
        let schema: Value =
            serde_json::from_str(&row.input_schema).unwrap_or_else(|_| json!({"type": "object"}));
        all_orca.push(json!({
            "name": row.fq_name,
            "description": row.description,
            "inputSchema": schema,
        }));
    }
    all_orca
}

/// GET the running daemon's live MCP catalog (`{version, tools}`). Returns
/// `None` on any transport/auth failure so callers fall back to the compiled-in
/// `core_tool_catalog()`. This is what keeps a stale `mcp-serve` child honest.
async fn fetch_daemon_catalog() -> Option<(String, Vec<Value>)> {
    let url = format!("{}/api/mcp/catalog", plugin_tool_http_base());
    let token = auth::loopback_token::get()
        .map(|s| s.to_string())
        .or_else(auth::loopback_token::read_from_disk)?;
    let client = auth::loopback_token::loopback_only_reqwest_client(&url).ok()?;
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let payload: Value = resp.json().await.ok()?;
    let version = payload.get("version").and_then(Value::as_str)?.to_string();
    let tools = payload.get("tools").and_then(Value::as_array)?.clone();
    Some((version, tools))
}

/// The live daemon's reported version, or `None` if it can't be reached.
async fn daemon_version() -> Option<String> {
    fetch_daemon_catalog().await.map(|(v, _)| v)
}

/// Forward a core (non-plugin, non-federated) tool call to the daemon's
/// `/api/v1/{name}` REST surface — the SAME registry this stdio child would
/// dispatch in-process, but executed by the LIVE binary. Used for tools this
/// (possibly stale) child does not recognize locally, so a self-update's new
/// tools are callable without a bridge restart.
///
/// Returns `Ok(None)` when the daemon also doesn't know the tool (so the caller
/// can fall through to legacy federation dispatch), `Ok(Some(result))` on
/// success, and `Err` on transport failure.
async fn call_core_tool_via_daemon(name: &str, args: &Value) -> Result<Option<Value>> {
    use anyhow::Context;
    let url = format!("{}/api/v1/{name}", plugin_tool_http_base());
    let token = auth::loopback_token::get()
        .map(|s| s.to_string())
        .or_else(auth::loopback_token::read_from_disk)
        .context("loopback token unavailable — is the daemon running?")?;
    // Forward args verbatim: the daemon's `http_dispatch` performs the same
    // ambient-peer / correlation-id overlay this child would, off the body.
    let resp = auth::loopback_token::loopback_only_reqwest_client(&url)?
        .post(&url)
        .bearer_auth(token)
        .json(args)
        .timeout(PLUGIN_TOOL_CALL_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let payload: Value = resp.json().await.context("tool response was not JSON")?;
    if status.is_success() {
        return Ok(Some(payload));
    }
    // A `tool.unknown` from the daemon means fall through to legacy dispatch.
    let code = payload.get("code").and_then(Value::as_str).unwrap_or("");
    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
    if status.as_u16() == 404 || code == "tool.unknown" || kind == "not_found" {
        return Ok(None);
    }
    let msg = payload
        .get("message")
        .or_else(|| payload.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    anyhow::bail!("tool '{name}' failed ({}): {msg}", status.as_u16());
}

// Context7 federation dispatch. All other tools flow through
// `orca_dispatch`'s inventory and never reach this match.
async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
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
    fn tool_names_hash_is_order_independent_and_change_sensitive() {
        // Reordering the same set must not trigger a spurious list_changed.
        assert_eq!(
            tool_names_hash(&["a", "b", "c"]),
            tool_names_hash(&["c", "a", "b"])
        );
        // Adding a tool (the self-update case) must change the signature.
        assert_ne!(
            tool_names_hash(&["a", "b"]),
            tool_names_hash(&["a", "b", "storage.mount.create"])
        );
    }

    #[test]
    fn core_tool_catalog_entries_are_well_formed() {
        // The daemon serves this to the stdio bridge; every entry must carry a
        // name + inputSchema so tools/list is a valid MCP catalog. (Federation
        // is merged client-side and is intentionally absent here.)
        let cat = core_tool_catalog();
        assert!(!cat.is_empty(), "registry should yield at least one tool");
        for t in &cat {
            assert!(
                t.get("name").and_then(Value::as_str).is_some(),
                "tool missing name: {t}"
            );
            assert!(
                t.get("inputSchema").is_some(),
                "tool missing inputSchema: {t}"
            );
        }
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

    // ── JSON-RPC framing: assert on the serialized wire bytes, not Value
    // indexing — the client parses a UTF-8 string, so that is the real
    // contract. ────────────────────────────────────────────────────────────

    #[test]
    fn reply_serializes_numeric_id_verbatim() {
        let wire = serde_json::to_string(&reply(json!(7), json!({ "ok": true }))).unwrap();
        // JSON-RPC 2.0 envelope, numeric id preserved (not stringified), and
        // the result nested under `result`.
        assert_eq!(
            wire, r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
            "reply wire shape drifted: {wire}"
        );
    }

    #[test]
    fn reply_preserves_string_and_null_ids() {
        let s = serde_json::to_string(&reply(json!("abc"), json!(null))).unwrap();
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":"abc","result":null}"#);
        // A null id is legal (matches the request's missing/None id fallback in
        // the serve loop) and must serialize as JSON null, never the string
        // "null".
        let n = serde_json::to_string(&reply(json!(null), json!({}))).unwrap();
        assert_eq!(n, r#"{"jsonrpc":"2.0","id":null,"result":{}}"#);
    }

    #[test]
    fn error_reply_serializes_code_and_message() {
        let wire = serde_json::to_string(&error_reply(json!(3), -32601, "method not found: bogus"))
            .unwrap();
        assert_eq!(
            wire,
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"method not found: bogus"}}"#,
            "error_reply wire shape drifted: {wire}"
        );
        // An error reply must never carry a `result` member — the two are
        // mutually exclusive in JSON-RPC 2.0.
        assert!(
            !wire.contains("\"result\""),
            "error must not contain result: {wire}"
        );
    }

    #[test]
    fn tool_names_hash_empty_differs_from_populated() {
        // The watcher seeds an empty baseline; the first real catalog must read
        // as a change.
        assert_ne!(tool_names_hash(&[]), tool_names_hash(&["host.detail"]));
        // Two independent empty sets hash equal (stable baseline).
        assert_eq!(tool_names_hash(&[]), tool_names_hash(&[]));
    }

    #[test]
    fn tool_names_hash_multiplicity_is_significant() {
        // Multiset semantics: a duplicated name is not the same as a single
        // one. Real catalogs never duplicate, but the hash must not silently
        // collapse them (which could mask a malformed catalog).
        assert_ne!(tool_names_hash(&["a"]), tool_names_hash(&["a", "a"]));
    }

    #[test]
    fn core_tool_catalog_names_are_unique() {
        // The daemon merges federation on top of this list keyed by name; a
        // duplicate here would let one entry shadow another. Registry, static,
        // and plugin sources must not collide.
        let cat = core_tool_catalog();
        let mut names: Vec<&str> = cat.iter().filter_map(|t| t["name"].as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            total,
            names.len(),
            "duplicate tool name in core_tool_catalog"
        );
    }

    #[test]
    fn core_tool_catalog_round_trips_as_json() {
        // Every entry is emitted verbatim into `tools/list`; each must be
        // serializable (no NaN/invalid) and re-parseable — the bridge writes
        // these straight to the wire.
        for t in &core_tool_catalog() {
            let s = serde_json::to_string(t).expect("catalog entry must serialize");
            assert!(s.starts_with('{'), "entry must be a JSON object: {s}");
            let round: Value = serde_json::from_str(&s).expect("catalog entry must re-parse");
            assert_eq!(&round, t, "catalog entry not round-trip stable");
        }
    }

    #[test]
    fn plugin_tool_http_base_has_https_scheme_and_port() {
        let base = plugin_tool_http_base();
        assert!(
            base.starts_with("https://"),
            "must be HTTPS loopback: {base}"
        );
        // A concrete port must be present so the URL is dialable.
        let port = base.rsplit(':').next().unwrap_or("");
        assert!(
            port.parse::<u16>().is_ok(),
            "loopback base must end in a numeric port: {base}"
        );
    }

    #[test]
    fn is_plugin_tool_false_for_unregistered_name() {
        // A fully-qualified name that no plugin declares must not be treated as
        // a plugin tool (it would otherwise be forwarded to the wrong dispatch
        // path). Holds whether or not a db is present in the test env.
        assert!(!is_plugin_tool("definitely.not.a.real.plugin.tool"));
    }

    #[test]
    fn load_plugin_tool_rows_never_panics() {
        // Robustness: a missing/locked db must degrade to an empty list, not a
        // panic — the bridge falls back to the registry-only catalog.
        let _rows = load_plugin_tool_rows();
    }

    #[tokio::test]
    async fn dispatch_rejects_unknown_tool() {
        // Legacy context7 federation dispatch only knows two names; anything
        // else must error out (never silently succeed) without touching the
        // network.
        let cfg = Config::load().expect("config load");
        let err = dispatch("nope_not_a_tool", &json!({}), &cfg)
            .await
            .expect_err("unknown tool must error");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown tool") && msg.contains("nope_not_a_tool"),
            "error must name the offending tool: {msg}"
        );
    }

    // Take a paired (catalog, daemon_version) observation that is internally
    // stable: because both are independent live probes of the same daemon, a
    // reachability transition *between* the two calls would produce an
    // inconsistent snapshot. Re-read until two consecutive snapshots agree on
    // availability (a window with no transition), so the caller asserts the real
    // invariant against a coherent observation rather than a torn one. Returns
    // the stable snapshot; the invariant assertions themselves are NOT relaxed.
    // Version-only projections of the three live daemon probes, so callers can
    // assert the real invariants without naming the catalog's opaque payload
    // type. Returns each probe's version string (None when the probe found no
    // daemon).
    #[cfg(test)]
    async fn stable_catalog_and_version_versions() -> (Option<String>, Option<String>) {
        let mut prev_present = None;
        for _ in 0..8 {
            let cat = fetch_daemon_catalog().await.map(|(v, _)| v);
            let ver = daemon_version().await;
            let present = (cat.is_some(), ver.is_some());
            if prev_present == Some(present) {
                return (cat, ver);
            }
            prev_present = Some(present);
        }
        (
            fetch_daemon_catalog().await.map(|(v, _)| v),
            daemon_version().await,
        )
    }

    #[cfg(test)]
    async fn stable_signature_and_catalog_versions() -> (Option<String>, Option<String>) {
        let mut prev_present = None;
        for _ in 0..8 {
            let sig = catalog_signature().await.map(|(v, _)| v);
            let cat = fetch_daemon_catalog().await.map(|(v, _)| v);
            let present = (sig.is_some(), cat.is_some());
            if prev_present == Some(present) {
                return (sig, cat);
            }
            prev_present = Some(present);
        }
        (
            catalog_signature().await.map(|(v, _)| v),
            fetch_daemon_catalog().await.map(|(v, _)| v),
        )
    }

    #[tokio::test]
    async fn fetch_daemon_catalog_is_consistent_with_daemon_version() {
        // Daemon-independent invariant: whatever the environment (daemon up or
        // down), a returned catalog must carry a non-empty version and its
        // version must equal what `daemon_version` reports from the same seam.
        ::model::ensure_crypto_provider();
        let (cat_ver, daemon_ver) = stable_catalog_and_version_versions().await;
        if let Some(version) = cat_ver {
            assert!(
                !version.is_empty(),
                "daemon catalog version must be non-empty"
            );
            assert_eq!(
                daemon_ver.as_deref(),
                Some(version.as_str()),
                "daemon_version must mirror fetch_daemon_catalog's version"
            );
        }
    }

    #[tokio::test]
    async fn catalog_signature_tracks_catalog_presence() {
        // The watcher only fires on a real change; a signature is present iff a
        // catalog is fetchable, so the two seams must agree on availability.
        ::model::ensure_crypto_provider();
        let (sig_ver, cat_ver) = stable_signature_and_catalog_versions().await;
        assert_eq!(
            sig_ver.is_some(),
            cat_ver.is_some(),
            "signature availability must match catalog availability"
        );
        if let (Some(sig_ver), Some(cat_ver)) = (sig_ver, cat_ver) {
            assert_eq!(
                sig_ver, cat_ver,
                "signature version must match catalog version"
            );
        }
    }

    #[tokio::test]
    async fn call_plugin_tool_errors_for_bogus_tool() {
        // A plugin tool that cannot exist never resolves to a success value:
        // either the loopback token/daemon is unavailable (transport error) or
        // the daemon rejects the unknown tool. Both are `Err`.
        ::model::ensure_crypto_provider();
        let res = call_plugin_tool("nonexistent.bogus_tool", &json!({})).await;
        assert!(res.is_err(), "bogus plugin tool must not succeed: {res:?}");
    }

    #[tokio::test]
    async fn call_core_tool_via_daemon_never_succeeds_for_unknown_tool() {
        // Stable across daemon up/down: an unknown core tool must resolve to
        // "fall through to legacy dispatch" (Ok(None)) or a transport error
        // (Err) — it must NEVER return Ok(Some(_)), which would fabricate a
        // successful result for a tool that does not exist.
        ::model::ensure_crypto_provider();
        let res = call_core_tool_via_daemon("definitely_not_a_registered_tool", &json!({})).await;
        assert!(
            !matches!(res, Ok(Some(_))),
            "unknown core tool must never yield a success payload: {res:?}"
        );
    }
}
