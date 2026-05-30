//! MCP-driven spec sync — connects to an MCP server, calls its
//! `{prefix}_spec_list` + `{prefix}_spec_schema` tools, upserts each advertised
//! repo into `orca.db`. MCP tool payloads are opaque upstream JSON.
#![allow(clippy::disallowed_types)] // MCP tool responses are arbitrary upstream JSON.

use super::SyncMcpSpecsResult;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

fn make_mcp_pool() -> ::mcp::client::McpPool {
    use contract::config::{APP_DB_FILE, APP_STATE_DIR};
    if let Ok(path) = std::env::var("ORCA_DB_PATH") {
        return ::mcp::client::McpPool::new_with_db(std::path::PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        return ::mcp::client::McpPool::new_with_db(home.join(APP_STATE_DIR).join(APP_DB_FILE));
    }
    ::mcp::client::McpPool::new()
}

pub async fn sync_mcp_specs(server: &str) -> Result<SyncMcpSpecsResult> {
    let pool = make_mcp_pool();
    let prefix = server.split('-').next().unwrap_or(server).to_string();
    let list_tool = format!("{prefix}_spec_list");
    let client = pool
        .get_or_connect(server)
        .await
        .with_context(|| format!("connect MCP server '{server}'"))?;

    let list_result = client
        .call_tool(&list_tool, json!({}), "sync-mcp")
        .await
        .with_context(|| format!("{list_tool} failed"))?;

    let text = list_result["content"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find_map(|c| c["text"].as_str().map(str::to_string))
        })
        .unwrap_or_default();

    let repos: Vec<String> = if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) {
        arr.into_iter()
            .filter_map(|v| {
                v["repo"]
                    .as_str()
                    .or_else(|| v["name"].as_str())
                    .or_else(|| v.as_str())
                    .map(str::to_string)
            })
            .collect()
    } else {
        text.lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches("• ")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|s| !s.is_empty() && !s.contains(':'))
            .collect()
    };

    if repos.is_empty() {
        return Err(anyhow!("MCP spec list returned no repos"));
    }

    let schema_tool = format!("{prefix}_spec_schema");
    let conn = db::open_default()?;
    let mut synced = 0u32;
    let mut errors: Vec<String> = Vec::new();

    for repo in &repos {
        if repo.is_empty() {
            continue;
        }
        match client
            .call_tool(&schema_tool, json!({ "repo": repo }), "sync-mcp")
            .await
        {
            Err(e) => errors.push(format!("{repo}: {e}")),
            Ok(r) => {
                let spec_text = r["content"].as_array().and_then(|arr| {
                    arr.iter()
                        .find_map(|c| c["text"].as_str().map(str::to_string))
                });
                let Some(spec_text) = spec_text else {
                    errors.push(format!("{repo}: empty schema response"));
                    continue;
                };
                if serde_json::from_str::<Value>(&spec_text).is_err() {
                    errors.push(format!("{repo}: non-JSON schema"));
                    continue;
                }
                let row = db::openapi_specs::OpenApiSpecRow {
                    name: repo.clone(),
                    url: None,
                    source_mcp: Some(prefix.clone()),
                    spec_json: Some(spec_text),
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    enabled: true,
                };
                match db::openapi_specs::upsert(&conn, &row) {
                    Ok(_) => synced += 1,
                    Err(e) => errors.push(format!("{repo}: db error: {e}")),
                }
            }
        }
    }

    Ok(SyncMcpSpecsResult {
        server: server.to_string(),
        synced,
        errors,
    })
}
