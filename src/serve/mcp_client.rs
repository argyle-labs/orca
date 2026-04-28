use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

#[derive(Clone, serde::Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
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
        let mut child = tokio::process::Command::new(&cfg.command)
            .args(&cfg.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;

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
        client.notify("notifications/initialized", json!({})).await?;

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
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let line = serde_json::to_string(&msg)? + "\n";
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let resp = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("MCP error: {err}");
        }
        Ok(resp["result"].clone())
    }
}

pub struct McpPool {
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
}

impl McpPool {
    pub fn new() -> Self {
        McpPool {
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub fn read_configs() -> HashMap<String, McpServerConfig> {
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
                Some((k.clone(), McpServerConfig { command, args }))
            })
            .collect()
    }

    pub async fn get_or_connect(&self, server_name: &str) -> Result<Arc<McpClient>> {
        let mut clients = self.clients.lock().await;
        if let Some(c) = clients.get(server_name) {
            return Ok(c.clone());
        }
        let configs = Self::read_configs();
        let cfg = configs
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("unknown MCP server: {server_name}"))?;
        let client = Arc::new(McpClient::connect(cfg).await?);
        clients.insert(server_name.to_string(), client.clone());
        Ok(client)
    }

    pub async fn all_tools(&self) -> Vec<Value> {
        let configs = Self::read_configs();
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

    pub async fn find_ctx7_server(&self) -> Option<String> {
        let configs = Self::read_configs();
        for server_name in configs.keys() {
            if let Ok(client) = self.get_or_connect(server_name).await {
                if client.tools.iter().any(|t| t.name == "resolve-library-id") {
                    return Some(server_name.clone());
                }
            }
        }
        None
    }
}
