#![allow(clippy::disallowed_types)] // mirrors trait signature — see trait-level allow
use anyhow::Result;
use contract::config::Config;
use serde_json::Value;

/// Proxy a context7 tool call through the configured context7 MCP server.
/// Discovers the server dynamically from the DB-backed McpPool.
pub async fn proxy_context7(tool: &str, args: &Value, config: &Config) -> Result<String> {
    use crate::client::McpPool;

    let pool = McpPool::new_with_db(config.db_path.clone());
    let server_name = pool.find_ctx7_server().await.ok_or_else(|| {
        anyhow::anyhow!("context7 not found — install the context7 plugin via the SDK plugin flow")
    })?;

    let client = pool.get_or_connect(&server_name).await?;
    let result = client.call_tool(tool, args.clone(), "orca-mcp").await?;

    let text = result["content"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v["text"].as_str())
        .unwrap_or("")
        .to_string();

    Ok(text)
}
