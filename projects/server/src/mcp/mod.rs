/// MCP stdio server — exposes brain tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: brain mcp-serve
/// Register: claude mcp add brain-local -- brain mcp-serve
mod context7;
mod docs;
mod handlers;
mod specs;
mod tools;

use anyhow::Result;
use brain_utils::config::Config;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use docs::{get_tree, list_commands, list_roots, read_doc, search_docs};
use handlers::{
    agents, get_agent, get_context, list_services, run, run_tests, search_logs, service_logs,
};
use specs::{get_graphql_info, get_rebuy_graphql_schema, get_rebuy_spec, get_rebuy_spec_public, list_rebuy_specs};

pub async fn serve(config: &Config) -> Result<()> {
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
                    "serverInfo": { "name": "brain", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, json!({ "tools": tools::tool_defs() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                let result = dispatch(name, args, config).await;
                match result {
                    Ok(text) => reply(
                        id,
                        json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
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

async fn dispatch(name: &str, args: &Value, config: &Config) -> Result<String> {
    match name {
        "brain_agents" => agents(),
        "brain_get_agent" => get_agent(args, config),
        "brain_run" => run(args, config).await,
        "brain_search_logs" => search_logs(args, config),
        "brain_get_context" => get_context(args, config),
        "list_roots" => list_roots(config),
        "get_tree" => get_tree(args, config),
        "read_doc" => read_doc(args, config),
        "search_docs" => search_docs(args, config),
        "list_commands" => list_commands(config),
        "brain_list_services" => list_services().await,
        "brain_service_logs" => service_logs(args).await,
        "brain_run_tests" => run_tests(args).await,
        "list_rebuy_specs" => list_rebuy_specs(),
        "get_rebuy_spec" => get_rebuy_spec(args),
        "get_rebuy_spec_public" => get_rebuy_spec_public(args),
        "get_rebuy_graphql_schema" => get_rebuy_graphql_schema(args),
        "get_graphql_info" => get_graphql_info(args),
        "resolve-library-id" | "get-library-docs" => {
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
