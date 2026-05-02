/// MCP stdio server — exposes orca tools to Claude Code via JSON-RPC 2.0.
///
/// Usage: orca mcp-serve
/// Register: claude mcp add orca-local -- orca mcp-serve
mod context7;
mod docs;
mod handlers;
mod specs;
mod tools;

use anyhow::Result;
use brain_utils::config::Config;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use docs::{get_tree, list_commands, list_roots, read_doc, search_docs};
use handlers::{
    agents, docker_add_runtime, docker_list_runtimes, docker_remove_runtime, get_agent, get_config,
    get_context, list_services, mcp_add_server, mcp_list_servers, mcp_list_mappings,
    mcp_map_tool, mcp_remove_server, mcp_sync_tools, mcp_unmap_tool, run,
    run_tests, schema_add_database, schema_list_databases, schema_remove_database, search_logs,
    service_logs,
};
use specs::{
    get_graphql_info, get_rebuy_graphql_schema, get_rebuy_spec, get_rebuy_spec_public,
    list_rebuy_specs, spec_refresh, spec_register, spec_unregister,
};

/// Servers whose tools brain already exposes natively or that must not be proxied back.
/// - orca-local: orca itself — proxying would spawn a recursive child
/// - context7: brain exposes resolve-library-id / get-library-docs natively
const FEDERATION_SKIP: &[&str] = &["orca-local", "context7"];

pub async fn serve(config: &Config) -> Result<()> {
    let pool = crate::serve::mcp_client::McpPool::new_with_db(config.db_path.clone());
    // Maps federated tool name → owning server name; populated on tools/list
    let mut tool_registry: HashMap<String, String> = HashMap::new();

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
                let orca_tools = tools::tool_defs();
                let orca_names: std::collections::HashSet<&str> = orca_tools
                    .as_array()
                    .map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect())
                    .unwrap_or_default();

                // Discover tools from federated servers, skipping orca-local and context7
                let external = pool.all_tools_filtered(FEDERATION_SKIP).await;

                // Rebuild registry: federated tools that don't conflict with orca's own
                tool_registry.clear();
                for tool in &external {
                    let name = tool["name"].as_str().unwrap_or("");
                    let server = tool["server"].as_str().unwrap_or("");
                    if !name.is_empty() && !server.is_empty() && !orca_names.contains(name) {
                        tool_registry.insert(name.to_string(), server.to_string());
                    }
                }

                // Merge orca tools + federated tools (strip internal "server" field)
                let mut all_tools: Vec<Value> =
                    orca_tools.as_array().cloned().unwrap_or_default();
                for mut tool in external {
                    let name = tool["name"].as_str().unwrap_or("").to_string();
                    if tool_registry.contains_key(&name) {
                        if let Some(obj) = tool.as_object_mut() {
                            obj.remove("server");
                        }
                        all_tools.push(tool);
                    }
                }

                reply(id, json!({ "tools": all_tools }))
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];

                if let Some(server_name) = tool_registry.get(name).cloned() {
                    // Route to the owning federated server
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
                            match client.call_tool(name, args.clone(), &cid).await {
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
                } else {
                    // Brain's own tools
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
        "orca_agents" => agents(),
        "orca_get_agent" => get_agent(args, config),
        "orca_run" => run(args, config).await,
        "orca_search_logs" => search_logs(args, config),
        "orca_get_config" => get_config(args, config),
        "orca_get_context" => get_context(args, config),
        "list_roots" => list_roots(config),
        "get_tree" => get_tree(args, config),
        "read_doc" => read_doc(args, config),
        "search_docs" => search_docs(args, config),
        "list_commands" => list_commands(config),
        "orca_list_services" => list_services().await,
        "orca_service_logs" => service_logs(args).await,
        "orca_run_tests" => run_tests(args).await,
        "list_rebuy_specs" => list_rebuy_specs(),
        "get_rebuy_spec" => get_rebuy_spec(args),
        "get_rebuy_spec_public" => get_rebuy_spec_public(args),
        "get_rebuy_graphql_schema" => get_rebuy_graphql_schema(args),
        "get_graphql_info" => get_graphql_info(args),
        "orca_mcp_list" => mcp_list_servers(),
        "orca_mcp_add" => mcp_add_server(args),
        "orca_mcp_remove" => mcp_remove_server(args),
        "orca_mcp_map" => mcp_map_tool(args),
        "orca_mcp_unmap" => mcp_unmap_tool(args),
        "orca_mcp_sync" => mcp_sync_tools(args),
        "orca_mcp_mappings" => mcp_list_mappings(args),
        "orca_schema_list" => schema_list_databases(),
        "orca_schema_add" => schema_add_database(args),
        "orca_schema_remove" => schema_remove_database(args),
        "orca_docker_list" => docker_list_runtimes(),
        "orca_docker_add" => docker_add_runtime(args),
        "orca_docker_remove" => docker_remove_runtime(args),
        "orca_spec_register" => spec_register(args).await,
        "orca_spec_refresh"  => spec_refresh(args).await,
        "orca_spec_unregister" => spec_unregister(args),
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
