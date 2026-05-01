use std::collections::HashMap;
use std::sync::Arc;

fn colima_docker_host() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let sock = format!("{home}/.colima/default/docker.sock");
    if std::path::Path::new(&sock).exists() {
        Some(format!("unix://{sock}"))
    } else {
        None
    }
}

/// Resolve a bare command name to an absolute path.
///
/// Launchd and other minimal environments strip PATH down to system directories,
/// so `node`, `npx`, etc. won't be found even when they're installed. Try `which`
/// first (works in interactive shells), then probe well-known install locations.
fn resolve_command(command: &str) -> String {
    if command.starts_with('/') {
        return command.to_string();
    }
    // which works when PATH is rich (interactive shell, dev mode)
    if let Ok(out) = std::process::Command::new("which").arg(command).output() {
        if out.status.success() {
            let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !resolved.is_empty() && std::path::Path::new(&resolved).exists() {
                return resolved;
            }
        }
    }
    // Probe known install paths — covers launchd/systemd daemon environments
    let mut candidates: Vec<String> = vec![
        format!("/opt/homebrew/bin/{command}"),  // Apple Silicon Homebrew
        format!("/usr/local/bin/{command}"),     // Intel Homebrew + manual installs
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
    tracing::warn!("could not resolve '{command}' to an absolute path; using as-is (may fail in daemon mode)");
    command.to_string()
}

use anyhow::Result;
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
}

pub struct McpClient {
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    request_lock: Mutex<()>,
    _child: Child,
    next_id: Mutex<u64>,
    pub tools: Vec<McpTool>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> Result<Self> {
        let resolved = resolve_command(&cfg.command);
        let mut cmd = tokio::process::Command::new(&resolved);
        cmd.args(&cfg.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        // Inject DOCKER_HOST so child processes (e.g. rebuy-cli running docker
        // compose) find the Colima socket instead of Docker Desktop's.
        if let Some(host) = colima_docker_host() {
            cmd.env("DOCKER_HOST", host);
        }

        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn()?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        let mut client = McpClient {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(stdout),
            request_lock: Mutex::new(()),
            _child: child,
            next_id: Mutex::new(0),
            tools: vec![],
        };

        // Initialize
        let init_resp = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "brain", "version": "1.0" }
                }),
            )
            .await?;
        drop(init_resp);

        // Send initialized notification
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        // List tools
        let tools_resp = client.request("tools/list", json!({})).await?;
        let tools: Vec<McpTool> = tools_resp["result"]["tools"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|t| McpTool {
                name: t["name"].as_str().unwrap_or("").to_string(),
                description: t["description"].as_str().unwrap_or("").to_string(),
                input_schema: t["inputSchema"].clone(),
            })
            .collect();
        client.tools = tools;

        Ok(client)
    }

    async fn next_id(&self) -> u64 {
        let mut id = self.next_id.lock().await;
        let current = *id;
        *id += 1;
        current
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id().await;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&msg)? + "\n";

        // Serialize the full write→read cycle — concurrent requests would interleave
        // reads on a single stdio pipe, causing each to consume the other's response.
        let _guard = self.request_lock.lock().await;

        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let mut buf = String::new();
                let n = {
                    let mut stdout = self.stdout.lock().await;
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
            Err(_) => anyhow::bail!("MCP server closed"),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let line = serde_json::to_string(&msg)? + "\n";
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
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
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
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

impl McpPool {
    pub fn new() -> Self {
        McpPool { clients: Mutex::new(HashMap::new()), db_path: None }
    }

    pub fn new_with_db(db_path: std::path::PathBuf) -> Self {
        McpPool { clients: Mutex::new(HashMap::new()), db_path: Some(db_path) }
    }

    pub fn read_configs(&self) -> HashMap<String, McpServerConfig> {
        let mut configs = Self::read_claude_configs();

        // DB servers take precedence over ~/.claude.json
        if let Some(db_path) = &self.db_path {
            if let Ok(conn) = brain_utils::db::open(db_path) {
                if let Ok(rows) = brain_utils::db::list_mcp_servers(&conn) {
                    for row in rows {
                        configs.insert(row.name.clone(), McpServerConfig {
                            command: row.command,
                            args: row.args,
                            env: row.env,
                        });
                    }
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
                Some((k.clone(), McpServerConfig { command, args, env }))
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
    /// Use this when federating to exclude the brain server itself and any servers
    /// whose tools brain already exposes natively (e.g. context7).
    pub async fn all_tools_filtered(&self, skip: &[&str]) -> Vec<Value> {
        let configs = self.read_configs();
        let mut result = Vec::new();
        for server_name in configs.keys() {
            if skip.contains(&server_name.as_str()) {
                continue;
            }
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
