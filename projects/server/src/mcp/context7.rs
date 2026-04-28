use anyhow::Result;
use brain_utils::config::Config;
use serde_json::Value;

/// Proxy a context7 tool call through the configured context7 MCP server in brain.toml.
pub async fn proxy_context7(tool: &str, args: &Value, config: &Config) -> Result<String> {
    use crate::serve::mcp_client::{McpClient, McpServerConfig};

    let server_cfg = config
        .mcp_servers
        .iter()
        .find(|s| s.name == "context7")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "context7 not configured — add to ~/brain/config/brain.toml:\n\
                 [[mcp.servers]]\nname = \"context7\"\ncommand = \"npx\"\nargs = [\"-y\", \"@upstash/context7-mcp@latest\"]"
            )
        })?;

    let cfg = McpServerConfig {
        command: server_cfg.command.clone(),
        args: server_cfg.args.clone(),
        env: server_cfg.env.clone(),
    };

    let client = McpClient::connect(&cfg).await?;
    let result = client.call_tool(tool, args.clone(), "brain-mcp").await?;

    let text = result["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v["text"].as_str())
        .unwrap_or("")
        .to_string();

    Ok(text)
}
