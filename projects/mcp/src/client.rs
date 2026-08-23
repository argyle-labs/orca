#![allow(clippy::disallowed_types)] // MCP JSON-RPC protocol — opaque tool args/results
use std::collections::HashMap;
use std::sync::Arc;

/// Resolve a bare command name to an absolute path.
///
/// Launchd and other minimal environments strip PATH down to system directories,
/// so `node`, `npx`, etc. won't be found even when they're installed. Try `which`
/// first (works in interactive shells), then probe well-known install locations.
/// Build a PATH that includes all well-known tool install directories so that
/// processes spawned by orca (MCP servers and their children) can find CLIs
/// like `node`, `npx`, etc. even in minimal daemon environments.
fn augmented_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();

    let mut extra: Vec<String> = vec![
        format!("{home}/.local/bin"),
        format!("{home}/.volta/bin"),
        format!("{home}/.fnm/current/bin"),
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
    ];

    // Add bin dirs for ALL installed nvm node versions. This avoids having to
    // resolve the alias chain (e.g. "24" → "v24.15.0") which nvm handles lazily.
    let nvm_versions = format!("{home}/.nvm/versions/node");
    if let Ok(entries) = std::fs::read_dir(&nvm_versions) {
        for entry in entries.flatten() {
            let bin = entry.path().join("bin");
            if bin.is_dir() {
                extra.push(bin.to_string_lossy().into_owned());
            }
        }
    }

    let mut parts: Vec<&str> = current.split(':').filter(|s| !s.is_empty()).collect();
    for dir in extra.iter().rev() {
        if !parts.contains(&dir.as_str()) {
            parts.insert(0, dir);
        }
    }
    parts.join(":")
}

fn resolve_command(command: &str) -> String {
    if command.starts_with('/') {
        return command.to_string();
    }
    // which works when PATH is rich (interactive shell, dev mode)
    if let Some(resolved) = utils::path::which(command)
        && std::path::Path::new(&resolved).exists()
    {
        return resolved;
    }
    // Probe known install paths — covers launchd/systemd daemon environments
    let mut candidates: Vec<String> = vec![
        format!("/opt/homebrew/bin/{command}"), // Apple Silicon Homebrew
        format!("/usr/local/bin/{command}"),    // Intel Homebrew + manual installs
        format!("/usr/bin/{command}"),
        format!("/bin/{command}"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        // nvm: read the default alias to find the active version
        let nvm_default = format!("{home}/.nvm/alias/default");
        if let Ok(ver) = std::fs::read_to_string(&nvm_default) {
            let ver = ver.trim().to_string();
            candidates.push(format!("{home}/.nvm/versions/node/{ver}/bin/{command}"));
            if !ver.starts_with('v') {
                candidates.push(format!("{home}/.nvm/versions/node/v{ver}/bin/{command}"));
            }
        }
        candidates.push(format!("{home}/.local/bin/{command}"));
        candidates.push(format!("{home}/.volta/bin/{command}")); // Volta
        candidates.push(format!("{home}/.fnm/current/bin/{command}")); // fnm
    }
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return path.clone();
        }
    }
    tracing::warn!(
        "could not resolve '{command}' to an absolute path; using as-is (may fail in daemon mode)"
    );
    command.to_string()
}

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

#[derive(Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Bearer token for HTTP/SSE transport (resolved from token_env at config load time).
    pub token: Option<String>,
    /// Additional SSE URLs tried in order if `command` is an http URL that fails.
    /// Priority: command (index 0) → fallback_urls[0] → fallback_urls[1] → ...
    #[serde(default)]
    pub fallback_urls: Vec<String>,
}

// ── Transport backends ────────────────────────────────────────────────────────

enum Transport {
    Stdio {
        stdin: Mutex<ChildStdin>,
        stdout: Mutex<BufReader<ChildStdout>>,
        _child: Box<Child>,
    },
    /// HTTP/SSE transport (MCP over Server-Sent Events).
    /// Each request opens a fresh /sse connection, gets a session endpoint, POSTs
    /// the JSON-RPC message, then reads the response from that same SSE stream.
    /// This is stateless per-request and matches the MCP /sse + /message model.
    Sse {
        base_url: String,
        http: reqwest::Client,
    },
}

pub struct McpClient {
    transport: Transport,
    request_lock: Mutex<()>,
    next_id: Mutex<u64>,
    pub tools: Vec<McpTool>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: utils::json_schema::JsonSchemaNode,
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self> {
        if cfg.command.starts_with("http://") || cfg.command.starts_with("https://") {
            // Try each URL in priority order, returning the first that succeeds.
            let all_urls = std::iter::once(cfg.command.as_str())
                .chain(cfg.fallback_urls.iter().map(|s| s.as_str()));
            let mut last_err = anyhow::anyhow!("no URLs configured");
            for url in all_urls {
                let mut candidate = cfg.clone();
                candidate.command = url.to_string();
                candidate.fallback_urls = vec![];
                match Self::connect_sse(&candidate).await {
                    Ok(client) => return Ok(client),
                    Err(e) => {
                        tracing::debug!("MCP SSE failed for {url}: {e}");
                        last_err = e;
                    }
                }
            }
            Err(last_err)
        } else {
            Self::connect_stdio(cfg).await
        }
    }

    async fn connect_stdio(cfg: &McpServerConfig) -> Result<Self> {
        let resolved = resolve_command(&cfg.command);
        let mut cmd = tokio::process::Command::new(&resolved);
        cmd.args(&cfg.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            // Reap the federated child when the client (and its boxed
            // handle) drops. Without this, a dropped `McpClient` leaks the
            // stdio subprocess — it lingers until the parent exits, holding
            // its own RSS and any sockets/files it opened.
            .kill_on_drop(true);

        // Augment PATH so MCP server subprocesses can find tools (node, npx, etc.)
        // that live in nvm/volta/fnm/homebrew paths stripped by launchd/systemd daemons.
        cmd.env("PATH", augmented_path());

        // Plugin-exposed environment (the generic `subprocess_env` seam): any
        // loaded plugin can expose env to spawned subprocesses without core
        // knowing it exists — e.g. the docker plugin contributes DOCKER_HOST for
        // whichever runtime is registered + active. Applied BEFORE cfg.env so
        // an operator's explicit per-server value always wins.
        for (k, v) in contract::subprocess_env::collect() {
            cmd.env(k, v);
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .context("MCP child process missing stdin pipe")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .context("MCP child process missing stdout pipe")?,
        );

        let mut client = McpClient {
            transport: Transport::Stdio {
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(stdout),
                _child: Box::new(child),
            },
            request_lock: Mutex::new(()),
            next_id: Mutex::new(0),
            tools: vec![],
        };

        client.handshake().await?;
        Ok(client)
    }

    async fn connect_sse(cfg: &McpServerConfig) -> Result<Self> {
        let base_url = cfg.command.trim_end_matches('/').to_string();

        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = &cfg.token
            && !token.is_empty()
        {
            let val = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| anyhow::anyhow!("invalid token: {e}"))?;
            headers.insert(reqwest::header::AUTHORIZATION, val);
        }

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;

        // Probe with a health check before attempting handshake.
        let health = http
            .get(utils::url::join(&base_url, "health"))
            .send()
            .await?;
        if !health.status().is_success() {
            anyhow::bail!("SSE server health check failed: HTTP {}", health.status());
        }

        let mut client = McpClient {
            transport: Transport::Sse { base_url, http },
            request_lock: Mutex::new(()),
            next_id: Mutex::new(0),
            tools: vec![],
        };

        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&mut self) -> Result<()> {
        let init_resp = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "orca", "version": "1.0" }
                }),
            )
            .await?;
        drop(init_resp);

        self.notify("notifications/initialized", json!({})).await?;

        let tools_resp = self.request("tools/list", json!({})).await?;
        let tools: Vec<McpTool> = tools_resp["result"]["tools"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|t| McpTool {
                name: t["name"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().unwrap_or("").to_string(),
                input_schema: serde_json::from_value(t["inputSchema"].clone()).unwrap_or_default(),
            })
            .collect();
        self.tools = tools;
        Ok(())
    }

    async fn next_id(&self) -> u64 {
        let mut id = self.next_id.lock().await;
        let current = *id;
        *id += 1;
        current
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_timeout(method, params, 30).await
    }

    async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout_secs: u64,
    ) -> Result<Value> {
        let id = self.next_id().await;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let _guard = self.request_lock.lock().await;

        match &self.transport {
            Transport::Stdio { stdin, stdout, .. } => {
                let line = serde_json::to_string(&msg)? + "\n";
                {
                    let mut stdin = stdin.lock().await;
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.flush().await?;
                }
                match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
                    loop {
                        let mut buf = String::new();
                        let n = {
                            let mut stdout = stdout.lock().await;
                            stdout.read_line(&mut buf).await?
                        };
                        if n == 0 {
                            anyhow::bail!("MCP server closed");
                        }
                        let buf = buf.trim();
                        if buf.is_empty() {
                            continue;
                        }
                        let resp: Value = serde_json::from_str(buf)?;
                        if resp["id"] == id {
                            return Ok(resp);
                        }
                    }
                    #[allow(unreachable_code)]
                    Ok(Value::Null)
                })
                .await
                {
                    Ok(r) => r,
                    Err(_) => anyhow::bail!("MCP server timed out"),
                }
            }

            Transport::Sse { base_url, http } => {
                // Per-request SSE: open /sse, get session endpoint, POST request, read response.
                // Each request gets its own isolated session so responses can't cross.
                let sse_resp = http
                    .get(utils::url::join(base_url, "sse"))
                    .header("Accept", "text/event-stream")
                    .send()
                    .await?;

                if !sse_resp.status().is_success() {
                    anyhow::bail!("SSE open failed: HTTP {}", sse_resp.status());
                }

                let mut stream = sse_resp.bytes_stream();
                let mut buf = String::new();

                // Read until we get the `data: /message?sessionId=…` endpoint line.
                let session_post =
                    match tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while let Some(Ok(chunk)) = stream.next().await {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            for line in buf.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    return Ok::<_, anyhow::Error>(data.trim().to_string());
                                }
                            }
                        }
                        anyhow::bail!("SSE closed before endpoint event")
                    })
                    .await
                    {
                        Ok(Ok(path)) => path,
                        Ok(Err(e)) => return Err(e),
                        Err(_) => anyhow::bail!("SSE endpoint event timed out"),
                    };

                let post_url = if session_post.starts_with("http") {
                    session_post
                } else {
                    format!("{base_url}{session_post}")
                };

                http.post(&post_url).json(&msg).send().await?;

                match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
                    let mut buf = String::new();
                    while let Some(Ok(chunk)) = stream.next().await {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        for line in buf.lines() {
                            if let Some(data) = line.strip_prefix("data: ") {
                                let data = data.trim();
                                if data.is_empty() {
                                    continue;
                                }
                                let resp: Value = serde_json::from_str(data)?;
                                if resp["id"] == id {
                                    return Ok(resp);
                                }
                            }
                        }
                    }
                    anyhow::bail!("SSE stream closed before response")
                })
                .await
                {
                    Ok(r) => r,
                    Err(_) => anyhow::bail!("MCP SSE request timed out"),
                }
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });

        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let line = serde_json::to_string(&msg)? + "\n";
                let mut stdin = stdin.lock().await;
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
            Transport::Sse { base_url, http } => {
                // Notifications via SSE: open a session, POST the notification.
                // The peer will ignore notifications that aren't JSON-RPC requests
                // (no `id` field means no response expected). Fire and forget.
                if let Ok(sse_resp) = http
                    .get(utils::url::join(base_url, "sse"))
                    .header("Accept", "text/event-stream")
                    .send()
                    .await
                    && sse_resp.status().is_success()
                {
                    let mut stream = sse_resp.bytes_stream();
                    let mut buf = String::new();
                    // Read endpoint event.
                    let mut session_post = String::new();
                    _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while let Some(Ok(chunk)) = stream.next().await {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            for line in buf.lines() {
                                if let Some(data) = line.strip_prefix("data: ") {
                                    session_post = data.trim().to_string();
                                    return;
                                }
                            }
                        }
                    })
                    .await;
                    if !session_post.is_empty() {
                        let post_url = if session_post.starts_with("http") {
                            session_post
                        } else {
                            format!("{base_url}{session_post}")
                        };
                        _ = http.post(&post_url).json(&msg).send().await;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        correlation_id: &str,
    ) -> Result<Value> {
        tracing::trace!(
            correlation_id = %correlation_id,
            tool = %name,
            arguments = %arguments,
            "→ mcp call"
        );

        let resp = self
            .request_timeout(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                300, // 5 minutes — agent runs can take much longer than 30s
            )
            .await?;

        if let Some(err) = resp.get("error") {
            tracing::trace!(
                correlation_id = %correlation_id,
                tool = %name,
                error = %err,
                "← mcp error"
            );
            anyhow::bail!("MCP error: {err}");
        }

        let result = resp["result"].clone();
        tracing::trace!(
            correlation_id = %correlation_id,
            tool = %name,
            result = %result,
            "← mcp result"
        );

        Ok(result)
    }
}

pub struct McpPool {
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
    db_path: Option<std::path::PathBuf>,
}

impl Default for McpPool {
    fn default() -> Self {
        Self::new()
    }
}

impl McpPool {
    pub fn new() -> Self {
        McpPool {
            clients: Mutex::new(HashMap::new()),
            db_path: None,
        }
    }

    pub fn new_with_db(db_path: std::path::PathBuf) -> Self {
        McpPool {
            clients: Mutex::new(HashMap::new()),
            db_path: Some(db_path),
        }
    }

    pub fn read_configs(&self) -> HashMap<String, McpServerConfig> {
        let mut configs = Self::read_claude_configs();

        // DB servers take precedence over ~/.claude.json
        if let Some(db_path) = &self.db_path
            && let Ok(conn) = db::open(db_path)
        {
            if let Ok(rows) = crate::servers::list(&conn) {
                for row in rows {
                    configs.insert(
                        row.name.clone(),
                        McpServerConfig {
                            command: row.command,
                            args: row.args,
                            env: row.env,
                            token: None,
                            fallback_urls: vec![],
                        },
                    );
                }
            }
            // Enabled plugins that declare an MCP server are auto-federated.
            // Plugin entries take precedence over ~/.claude.json but not over explicit mcp_servers rows.
            if let Ok(plugins) = db::plugins::list(&conn) {
                for p in plugins {
                    if !p.enabled {
                        continue;
                    }

                    // Transport lives in the manifest, not the row — re-parse on demand.
                    let Ok((manifest, _)) = db::plugin_manifest::parse_path(&p.manifest_path)
                    else {
                        continue;
                    };
                    let Some(mcp) = manifest.plugin.mcp else {
                        continue;
                    };
                    // urls (priority-ordered list) override stdio command.
                    // All URLs are passed; connect() tries them in order.
                    let urls = mcp.urls();
                    let (cmd, fallback_urls) = if !urls.is_empty() {
                        let mut it = urls.into_iter();
                        let primary = it.next().unwrap();
                        (primary, it.collect::<Vec<_>>())
                    } else if let Some(c) = mcp.command_nonempty() {
                        (c.to_string(), vec![])
                    } else {
                        continue;
                    };
                    // Merge stored credentials (orca creds set) into env so the subprocess
                    // receives them without requiring the caller to export them manually.
                    let mut env = mcp.env;
                    let mut token: Option<String> = None;
                    if let Ok(creds) = db::plugin_creds::list(&conn, &p.id) {
                        for c in creds {
                            // If this credential matches token_env, use it as Bearer token.
                            if mcp.token_env.as_deref() == Some(c.key.as_str()) {
                                token = Some(c.value.clone());
                            }
                            env.insert(c.key, c.value);
                        }
                    }
                    configs.entry(p.id).or_insert(McpServerConfig {
                        command: cmd,
                        args: mcp.args,
                        env,
                        token,
                        fallback_urls,
                    });
                }
            }
        }

        configs
    }

    fn read_claude_configs() -> HashMap<String, McpServerConfig> {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = format!("{home}/.claude.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return HashMap::new();
        };
        let Ok(json): Result<Value, _> = serde_json::from_str(&raw) else {
            return HashMap::new();
        };
        let Some(servers) = json["mcpServers"].as_object() else {
            return HashMap::new();
        };
        servers
            .iter()
            .filter_map(|(k, v)| {
                let command = v["command"].as_str()?.to_string();
                let args = v["args"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|a| a.as_str().map(|s| s.to_string()))
                    .collect();
                let env = v["env"]
                    .as_object()
                    .map(|m| {
                        m.iter()
                            .filter_map(|(ek, ev)| ev.as_str().map(|s| (ek.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((
                    k.clone(),
                    McpServerConfig {
                        command,
                        args,
                        env,
                        token: None,
                        fallback_urls: vec![],
                    },
                ))
            })
            .collect()
    }

    pub async fn get_or_connect(&self, server_name: &str) -> Result<Arc<McpClient>> {
        let mut clients = self.clients.lock().await;
        if let Some(c) = clients.get(server_name) {
            return Ok(c.clone());
        }
        let configs = self.read_configs();
        let cfg = configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server: {server_name}"))?;
        let client = Arc::new(McpClient::connect(cfg).await?);
        clients.insert(server_name.to_string(), client.clone());
        Ok(client)
    }

    pub async fn evict(&self, server_name: &str) {
        self.clients.lock().await.remove(server_name);
    }

    pub async fn all_tools(&self) -> Vec<Value> {
        let configs = self.read_configs();
        let mut result = Vec::new();
        for server_name in configs.keys() {
            if let Ok(client) = self.get_or_connect(server_name).await {
                for tool in &client.tools {
                    result.push(json!({
                        "server": server_name,
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": tool.input_schema,
                    }));
                }
            }
        }
        result
    }

    /// Like `all_tools` but skips named servers entirely — avoids connecting to them.
    ///
    /// Naming logic per tool (in priority order):
    /// 1. Explicit override in plugin's `command_map` (universal → internal).
    /// 2. Auto-strip: if tool name starts with `{plugin_id}_`, strip that prefix.
    /// 3. Pass-through: expose tool under its original name.
    ///
    /// The `alias` field carries the internal tool name when a rename occurred,
    /// used by the federation router to call the right name on the remote server.
    pub async fn all_tools_filtered(&self, skip: &[&str]) -> Vec<Value> {
        // Per plugin: inverse command_map (internal_name → universal_name) + id prefix
        struct PluginMeta {
            prefix: String,                   // "{id}_" — stripped from tool names automatically
            inverse: HashMap<String, String>, // internal_name → explicit universal_name
        }

        let plugin_meta: HashMap<String, PluginMeta> = self
            .db_path
            .as_ref()
            .and_then(|p| db::open(p).ok())
            .and_then(|conn| db::plugins::list(&conn).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| {
                let prefix = format!("{}_", p.id);
                let inverse = p.command_map.into_iter().map(|(u, t)| (t, u)).collect();
                (p.id, PluginMeta { prefix, inverse })
            })
            .collect();

        let configs = self.read_configs();

        // Federate in parallel with a per-server hard deadline so that a single
        // unreachable server (e.g. an off-LAN homelab plugin) cannot block the
        // entire tools/list call. Servers that error or time out are silently
        // dropped — they simply don't appear in the federation set this call.
        let attempts = configs
            .keys()
            .filter(|n| !skip.contains(&n.as_str()))
            .cloned()
            .map(|name| async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    self.get_or_connect(&name),
                )
                .await
                {
                    Ok(Ok(client)) => Some((name, client)),
                    _ => None,
                }
            });
        let connected: Vec<(String, Arc<McpClient>)> = futures_util::future::join_all(attempts)
            .await
            .into_iter()
            .flatten()
            .collect();

        let mut result = Vec::new();
        for (server_name, client) in &connected {
            let meta = plugin_meta.get(server_name.as_str());
            {
                for tool in &client.tools {
                    let universal = if let Some(m) = meta {
                        if let Some(explicit) = m.inverse.get(&tool.name) {
                            // Explicit override wins
                            explicit.clone()
                        } else if let Some(stripped) = tool.name.strip_prefix(&m.prefix) {
                            // Auto-strip plugin id prefix
                            stripped.to_string()
                        } else {
                            // No prefix match — pass through as-is
                            tool.name.clone()
                        }
                    } else {
                        tool.name.clone()
                    };

                    if universal == tool.name {
                        result.push(json!({
                            "server": server_name,
                            "name": universal,
                            "description": tool.description,
                            "inputSchema": tool.input_schema,
                        }));
                    } else {
                        result.push(json!({
                            "server": server_name,
                            "name": universal,
                            "alias": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.input_schema,
                        }));
                    }
                }
            }
        }
        result
    }

    pub async fn find_ctx7_server(&self) -> Option<String> {
        let configs = self.read_configs();
        for server_name in configs.keys() {
            if let Ok(client) = self.get_or_connect(server_name).await
                && client.tools.iter().any(|t| t.name == "resolve-library-id")
            {
                return Some(server_name.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_command ───────────────────────────────────────────────────────

    #[test]
    fn resolve_command_absolute_path_returned_unchanged() {
        // Absolute paths bypass all resolution logic.
        assert_eq!(resolve_command("/usr/bin/env"), "/usr/bin/env");
        assert_eq!(resolve_command("/bin/bash"), "/bin/bash");
    }

    #[test]
    fn resolve_command_known_binary_returns_nonempty() {
        // "bash" exists on every CI/dev machine — we just need it to resolve to something.
        let resolved = resolve_command("bash");
        assert!(
            !resolved.is_empty(),
            "resolve_command('bash') should return non-empty"
        );
        // Should be an absolute path or the bare name unchanged
        assert!(
            resolved == "bash" || resolved.starts_with('/'),
            "got: {resolved}"
        );
    }

    #[test]
    fn resolve_command_unknown_returns_input_unchanged() {
        // A completely made-up command falls through all probes and returns as-is.
        let result = resolve_command("zzz_no_such_binary_xyz_999");
        assert_eq!(result, "zzz_no_such_binary_xyz_999");
    }

    // ── augmented_path ────────────────────────────────────────────────────────

    #[test]
    fn augmented_path_contains_homebrew_bin() {
        let path = augmented_path();
        // On macOS the output should include at least one of the standard dirs
        assert!(
            path.contains("/opt/homebrew/bin")
                || path.contains("/usr/local/bin")
                || path.contains("/usr/bin"),
            "augmented_path missing expected dirs: {path}",
        );
    }

    #[test]
    fn augmented_path_has_no_empty_segments() {
        let path = augmented_path();
        for segment in path.split(':') {
            assert!(!segment.is_empty(), "empty segment in PATH: {path}");
        }
    }

    #[test]
    fn augmented_path_does_not_add_duplicate_extra_dirs() {
        // The extras we inject should not appear twice.
        let path = augmented_path();
        let mut seen = std::collections::HashSet::new();
        for candidate in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"] {
            if path.contains(candidate) {
                assert!(
                    seen.insert(candidate),
                    "extra dir appears more than once: {candidate}"
                );
            }
        }
    }

    // ── kill_on_drop reaps the federated child ────────────────────────────────

    // `kill(pid, 0)` — probe for process existence. Declared inline rather
    // than pulling a `libc`/`nix` dep for one syscall, mirroring the
    // reconciler's raw-ESTALE-constant convention. Returns 0 while the pid
    // is live, -1 with errno=ESRCH once it's gone.
    #[cfg(unix)]
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    #[cfg(unix)]
    fn pid_is_gone(pid: u32) -> bool {
        // SAFETY: kill(pid, 0) performs error checking only — no signal is
        // delivered. errno is consulted via Error::last_os_error.
        let rc = unsafe { kill(pid as i32, 0) };
        if rc == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(/* ESRCH */ 3)
    }

    /// Dropping an `McpClient` whose stdio child was spawned with
    /// `kill_on_drop(true)` must reap that child rather than leaking it.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_stdio_client_kills_child() {
        // Spawn a trivial long-lived stdio child via the SAME builder path
        // `connect_stdio` uses (incl. `kill_on_drop(true)`). `cat` with a
        // piped stdin blocks forever waiting for input, so it can only exit
        // by being killed.
        let mut cmd = tokio::process::Command::new("cat");
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn cat");
        let pid = child.id().expect("child pid");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        let client = McpClient {
            transport: Transport::Stdio {
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(stdout),
                _child: Box::new(child),
            },
            request_lock: Mutex::new(()),
            next_id: Mutex::new(0),
            tools: vec![],
        };

        assert!(!pid_is_gone(pid), "precondition: child live before drop");
        drop(client);

        // kill_on_drop sends SIGKILL on drop; reaping is async. Poll briefly.
        for _ in 0..100 {
            if pid_is_gone(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("child pid {pid} still alive after client drop");
    }

    // ── McpServerConfig deserialization ───────────────────────────────────────

    #[test]
    fn server_config_deserializes_with_defaults() {
        // Only `command` + `args` supplied — env/token/fallback_urls default.
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "command": "node",
            "args": ["server.js", "--flag"],
        }))
        .expect("deserialize minimal config");

        assert_eq!(cfg.command, "node");
        assert_eq!(cfg.args, vec!["server.js", "--flag"]);
        assert!(cfg.env.is_empty(), "env should default empty");
        assert!(cfg.token.is_none(), "token should default None");
        assert!(
            cfg.fallback_urls.is_empty(),
            "fallback_urls should default empty"
        );
    }

    #[test]
    fn server_config_deserializes_full() {
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "command": "https://mcp.example.com",
            "args": [],
            "env": { "API_KEY": "abc" },
            "token": "secret-token",
            "fallback_urls": ["https://backup.example.com"],
        }))
        .expect("deserialize full config");

        assert_eq!(cfg.command, "https://mcp.example.com");
        assert_eq!(cfg.env.get("API_KEY").map(String::as_str), Some("abc"));
        assert_eq!(cfg.token.as_deref(), Some("secret-token"));
        assert_eq!(cfg.fallback_urls, vec!["https://backup.example.com"]);
    }

    // ── McpTool serde ─────────────────────────────────────────────────────────

    #[test]
    fn mcp_tool_round_trips_with_input_schema_rename() {
        let tool = McpTool {
            name: "echo".to_string(),
            description: "echoes input".to_string(),
            input_schema: serde_json::from_value(json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } }
            }))
            .expect("schema node"),
        };

        let v = serde_json::to_value(&tool).expect("serialize tool");
        // The struct field `input_schema` must serialize as `inputSchema`.
        assert!(
            v.get("inputSchema").is_some(),
            "expected camelCase `inputSchema` key, got: {v}"
        );
        assert!(
            v.get("input_schema").is_none(),
            "snake_case key must not leak"
        );
        assert_eq!(v["name"], "echo");

        let back: McpTool = serde_json::from_value(v).expect("deserialize tool");
        assert_eq!(back.name, "echo");
        assert_eq!(back.description, "echoes input");
    }

    // ── McpPool constructors & simple accessors ───────────────────────────────

    #[tokio::test]
    async fn pool_new_is_empty_and_evict_is_noop() {
        let pool = McpPool::new();
        // Evicting from an empty pool must not panic.
        pool.evict("does-not-exist").await;

        let default_pool = McpPool::default();
        default_pool.evict("nope").await;
    }

    #[tokio::test]
    async fn pool_get_or_connect_unknown_server_errors() {
        // A pool with an isolated (non-existent) db path won't see any DB servers,
        // so a nonsense server name must resolve to an error, not a hang.
        let pool = McpPool::new_with_db(std::path::PathBuf::from(
            "/nonexistent/orca-test-db-xyz/orca.db",
        ));
        let err = match pool
            .get_or_connect("zzz-server-that-cannot-exist-999")
            .await
        {
            Ok(_) => panic!("unknown server should error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("unknown MCP server"),
            "unexpected error: {err}"
        );
    }

    // ── End-to-end stdio transport against a fake MCP server ──────────────────

    /// A minimal, protocol-correct MCP server over stdio, in Python. Responds to
    /// `initialize`, ignores notifications, returns two tools from `tools/list`,
    /// echoes `tools/call` arguments back, and returns a JSON-RPC error for the
    /// tool named `boom`. This exercises the full stdio request/response framing.
    #[cfg(unix)]
    fn write_fake_server() -> std::path::PathBuf {
        let script = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        # notification — no response expected
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "serverInfo": {"name": "fake", "version": "1.0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "echo", "description": "echo tool",
             "inputSchema": {"type": "object",
                             "properties": {"msg": {"type": "string"}}}},
            {"name": "resolve-library-id", "description": "ctx7 probe",
             "inputSchema": {"type": "object"}}
        ]}})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "boom":
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32000, "message": "boom failed"}})
        else:
            send({"jsonrpc": "2.0", "id": mid,
                  "result": {"content": [{"type": "text",
                                          "text": json.dumps(args)}]}})
    else:
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
"#;
        let mut path = std::env::temp_dir();
        let unique = format!(
            "orca-fake-mcp-{}-{}.py",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        std::fs::write(&path, script).expect("write fake server script");
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_connect_handshake_and_call_tool() {
        let script = write_fake_server();
        let cfg = McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env: std::collections::HashMap::new(),
            token: None,
            fallback_urls: vec![],
        };

        // connect() → connect_stdio() → handshake() (initialize + notify + tools/list).
        let client = McpClient::connect(&cfg).await.expect("connect to fake MCP");

        // handshake must have parsed both advertised tools.
        assert_eq!(client.tools.len(), 2, "expected two tools");
        assert!(client.tools.iter().any(|t| t.name == "echo"));
        assert!(
            client.tools.iter().any(|t| t.name == "resolve-library-id"),
            "ctx7 probe tool missing"
        );
        let echo = client
            .tools
            .iter()
            .find(|t| t.name == "echo")
            .expect("echo tool");
        assert_eq!(echo.description, "echo tool");

        // A successful tool call returns the `result` object verbatim.
        let result = client
            .call_tool("echo", json!({ "msg": "hi there" }), "corr-1")
            .await
            .expect("echo call should succeed");
        let text = result["content"][0]["text"].as_str().expect("text content");
        // The fake server echoes the arguments back as JSON text.
        let echoed: Value = serde_json::from_str(text).expect("parse echoed args");
        assert_eq!(echoed["msg"], "hi there");

        // A second call must use a fresh request id and still match its response.
        let result2 = client
            .call_tool("echo", json!({ "msg": "again" }), "corr-2")
            .await
            .expect("second echo call");
        let text2 = result2["content"][0]["text"].as_str().expect("text");
        let echoed2: Value = serde_json::from_str(text2).expect("parse");
        assert_eq!(echoed2["msg"], "again");

        // A JSON-RPC error response surfaces as an Err from call_tool.
        let err = client
            .call_tool("boom", json!({}), "corr-3")
            .await
            .expect_err("boom must error");
        assert!(
            err.to_string().contains("MCP error"),
            "unexpected error text: {err}"
        );

        drop(client);
        drop(std::fs::remove_file(&script));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn next_id_is_monotonic() {
        // Build a client directly around the fake server so we can probe next_id
        // without going through a full request cycle.
        let script = write_fake_server();
        let cfg = McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env: std::collections::HashMap::new(),
            token: None,
            fallback_urls: vec![],
        };
        let client = McpClient::connect(&cfg).await.expect("connect");

        // handshake already consumed ids 0 (initialize) and 1 (tools/list); the
        // exact starting value is an implementation detail, so assert monotonicity.
        let a = client.next_id().await;
        let b = client.next_id().await;
        let c = client.next_id().await;
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);

        drop(client);
        drop(std::fs::remove_file(&script));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connect_falls_through_stdio_for_non_url_command() {
        // A command that is not an http(s) URL routes to the stdio transport.
        // Point it at the fake server and confirm a working client comes back.
        let script = write_fake_server();
        let cfg = McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env: [("ORCA_TEST_ENV".to_string(), "1".to_string())]
                .into_iter()
                .collect(),
            token: None,
            fallback_urls: vec![],
        };
        let client = McpClient::connect(&cfg).await.expect("connect stdio");
        assert!(!client.tools.is_empty());
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    // ── Additional stdio fixtures ─────────────────────────────────────────────

    /// Write an arbitrary Python script to a uniquely-named temp file. Reuses the
    /// pid + nanos + counter naming to avoid collisions between parallel tests.
    #[cfg(unix)]
    fn write_script(body: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        let unique = format!(
            "orca-fake-mcp-{}-{}-{}.py",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        );
        path.push(unique);
        std::fs::write(&path, body).expect("write script");
        path
    }

    #[cfg(unix)]
    fn cfg_for(script: &std::path::Path) -> McpServerConfig {
        McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            env: std::collections::HashMap::new(),
            token: None,
            fallback_urls: vec![],
        }
    }

    // Handshake body shared by fixtures that need a working `initialize` +
    // `notifications/initialized` sink before diverging on later methods.
    #[cfg(unix)]
    const HANDSHAKE_PREAMBLE: &str = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
def handshake(msg, mid, method):
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{}}})
        return True
    if method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[]}})
        return True
    return False
"#;

    /// A request whose response never arrives must surface a timeout error, not
    /// hang. Exercises the `tokio::time::timeout` Err arm of the stdio branch.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_times_out_when_server_silent() {
        let body = HANDSHAKE_PREAMBLE.to_string()
            + r#"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if handshake(msg, mid, method):
        continue
    # Any other request: read but never respond.
"#;
        let script = write_script(&body);
        let client = McpClient::connect(&cfg_for(&script))
            .await
            .expect("connect");
        // Private helper: 1s timeout keeps the test fast.
        let err = client
            .request_timeout("tools/call", json!({ "name": "x", "arguments": {} }), 1)
            .await
            .expect_err("silent server must time out");
        assert!(
            err.to_string().contains("timed out"),
            "unexpected error: {err}"
        );
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    /// When the server closes its stdout mid-session, `read_line` returns 0 and
    /// the client reports the connection closed. Exercises the `n == 0` arm.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_errors_when_server_closes_stdout() {
        let body = HANDSHAKE_PREAMBLE.to_string()
            + r#"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if handshake(msg, mid, method):
        continue
    # Any other request: the server has already consumed the request line, so
    # exiting now closes stdout cleanly and the client's read sees EOF (n == 0)
    # rather than a partial response.
    break
"#;
        let script = write_script(&body);
        let client = McpClient::connect(&cfg_for(&script))
            .await
            .expect("connect");
        let err = client
            .request_timeout("tools/call", json!({ "name": "x", "arguments": {} }), 5)
            .await
            .expect_err("closed stdout must error");
        assert!(
            err.to_string().contains("closed"),
            "unexpected error: {err}"
        );
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    /// The response-matching loop must skip blank lines and messages whose `id`
    /// doesn't match the pending request, then return the correct one. Exercises
    /// both `continue` arms (empty buffer + id mismatch) before the match.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_request_skips_noise_and_mismatched_ids() {
        let body = HANDSHAKE_PREAMBLE.to_string()
            + r#"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if handshake(msg, mid, method):
        continue
    if method == "tools/call":
        # Emit a blank line, then a response with a non-matching id, then the
        # real response. The client must ignore the first two.
        sys.stdout.write("\n"); sys.stdout.flush()
        send({"jsonrpc":"2.0","id":999999,"result":{"stale":True}})
        send({"jsonrpc":"2.0","id":mid,"result":{"ok":True}})
    else:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;
        let script = write_script(&body);
        let client = McpClient::connect(&cfg_for(&script))
            .await
            .expect("connect");
        let result = client
            .call_tool("echo", json!({}), "corr-noise")
            .await
            .expect("call should skip noise and succeed");
        // Assert on the serialized result rather than a Value index.
        assert_eq!(
            serde_json::to_string(&result).expect("serialize result"),
            r#"{"ok":true}"#
        );
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    /// `handshake` must tolerate tool entries missing `description`/`inputSchema`
    /// (defaulting each) and a tool missing `name` (empty string). Exercises the
    /// `unwrap_or`/`unwrap_or_default` arms of the tools/list mapping.
    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_defaults_missing_tool_fields() {
        let body = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[
            {"name":"only-name"},
            {"description":"no name here","inputSchema":{"type":"object"}}
        ]}})
    else:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;
        let script = write_script(body);
        let client = McpClient::connect(&cfg_for(&script))
            .await
            .expect("connect");
        assert_eq!(client.tools.len(), 2, "both entries should map");
        let only = client
            .tools
            .iter()
            .find(|t| t.name == "only-name")
            .expect("only-name tool");
        assert_eq!(only.description, "", "missing description defaults empty");
        // Default schema serializes without error and carries no properties.
        let schema = serde_json::to_string(&only.input_schema).expect("serialize schema");
        assert!(
            !schema.contains("properties"),
            "default schema should have no properties: {schema}"
        );
        assert!(
            client.tools.iter().any(|t| t.name.is_empty()),
            "tool missing name should map to empty string"
        );
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    /// A tools/list result with no `tools` array leaves the client with an empty
    /// tool set. Exercises the `as_array().unwrap_or(&vec![])` arm.
    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_missing_tools_key_yields_no_tools() {
        let body = r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{}}})
    elif method == "tools/list":
        # No `tools` key at all.
        send({"jsonrpc":"2.0","id":mid,"result":{}})
    else:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#;
        let script = write_script(body);
        let client = McpClient::connect(&cfg_for(&script))
            .await
            .expect("connect");
        assert!(client.tools.is_empty(), "no tools key → empty tool set");
        drop(client);
        drop(std::fs::remove_file(&script));
    }

    // ── McpPool cache hit + eviction ──────────────────────────────────────────

    /// `get_or_connect` returns a cached client without reconnecting, and
    /// `evict` removes it from the pool. Exercises the cache-hit early return
    /// and the populated-map eviction path (the empty-map case is covered
    /// elsewhere).
    #[cfg(unix)]
    #[tokio::test]
    async fn pool_get_or_connect_returns_cached_then_evicts() {
        let script = write_script(
            r#"
import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[]}})
    else:
        send({"jsonrpc":"2.0","id":mid,"result":{}})
"#,
        );
        let client = Arc::new(
            McpClient::connect(&cfg_for(&script))
                .await
                .expect("connect"),
        );

        let pool = McpPool::new();
        pool.clients
            .lock()
            .await
            .insert("cached".to_string(), client.clone());

        // Cache hit: same Arc back, no reconnection (no config lookup needed).
        let got = pool.get_or_connect("cached").await.expect("cached client");
        assert!(
            Arc::ptr_eq(&got, &client),
            "get_or_connect must return the cached instance"
        );

        drop(got);
        pool.evict("cached").await;
        assert!(
            pool.clients.lock().await.is_empty(),
            "evict must remove the cached entry"
        );

        drop(client);
        drop(std::fs::remove_file(&script));
    }
}
