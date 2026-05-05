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
use orca_utils::config::Config;
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::serve::api::llm as local_llm;
use docs::{get_tree, list_commands, list_roots, read_doc, search_docs};
use handlers::{
    agent_backend_api_key_status, agent_backend_clear_api_key, agent_backend_override,
    agent_backend_set_api_key, agent_backend_set_mode, agent_backend_status,
    agent_backend_use_server_anthropic,
    agents, docker_add_runtime, docker_list_runtimes, docker_remove_runtime, get_agent, get_config,
    get_context, list_services, mcp_add_server, mcp_list_servers, mcp_list_mappings,
    mcp_map_tool, mcp_remove_server, mcp_sync_tools, mcp_unmap_tool,
    plugin_add, plugin_disable, plugin_enable, plugin_remove,
    plugin_creds_list, plugin_creds_remove, plugin_creds_set, plugin_creds_sync, plugin_list,
    run, run_tests, schema_add_database, schema_list_databases, schema_remove_database,
    search_logs, service_logs,
    doc_list_roots, doc_add_root, doc_remove_root,
    doc_list_ignore_patterns, doc_add_ignore_pattern, doc_remove_ignore_pattern,
};
use specs::{
    get_graphql_info, get_rebuy_graphql_schema, get_rebuy_spec, get_rebuy_spec_public,
    list_rebuy_specs, spec_refresh, spec_register, spec_unregister,
};

/// Servers whose tools orca already exposes natively or that must not be proxied back.
/// - orca-local: orca itself — proxying would spawn a recursive child
const FEDERATION_SKIP: &[&str] = &["orca-local"];

pub async fn serve(config: &Config) -> Result<()> {
    let pool = crate::serve::mcp_client::McpPool::new_with_db(config.db_path.clone());
    // Maps exposed tool name → (server_name, internal_tool_name).
    // For universal-mapped tools: exposed name differs from internal name.
    // For pass-through tools: both names are the same.
    let mut tool_registry: HashMap<String, (String, String)> = HashMap::new();

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

                // Rebuild registry: federated tools that don't conflict with orca's own.
                // alias = internal tool name on the remote server (may differ for mapped tools).
                tool_registry.clear();
                for tool in &external {
                    let name = tool["name"].as_str().unwrap_or("");
                    let server = tool["server"].as_str().unwrap_or("");
                    let alias = tool["alias"].as_str().unwrap_or(name);
                    if !name.is_empty() && !server.is_empty() && !orca_names.contains(name) {
                        tool_registry.insert(name.to_string(), (server.to_string(), alias.to_string()));
                    }
                }

                // Merge orca tools + federated tools (strip internal fields)
                let mut all_tools: Vec<Value> =
                    orca_tools.as_array().cloned().unwrap_or_default();
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

                if let Some((server_name, internal_name)) = tool_registry.get(name).cloned() {
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
                } else {
                    // Orca's own tools
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
        "list_agents"         => agents(),
        "get_agent"           => get_agent(args, config),
        "run_agent"           => run(args, config).await,
        "search_logs"         => {
            let raw = search_logs(args, config)?;
            let query = args["query"].as_str().unwrap_or_default();
            if let Some(llm) = local_llm::discover_local_llm().await {
                if let Some(enhanced) = local_llm::present_text_results(&llm, query, &raw, 8000).await {
                    return Ok(enhanced);
                }
            }
            Ok(raw)
        }
        "get_config"          => get_config(args, config),
        "get_context"         => get_context(args, config),
        "list_roots"          => list_roots(config),
        "get_tree"            => get_tree(args, config),
        "read_doc"            => read_doc(args, config),
        "search_docs"         => {
            let raw = search_docs(args, config)?;
            let query = args["query"].as_str().unwrap_or_default();
            if let Some(llm) = local_llm::discover_local_llm().await {
                if let Some(enhanced) = local_llm::present_text_results(&llm, query, &raw, 8000).await {
                    return Ok(enhanced);
                }
            }
            Ok(raw)
        }
        "list_commands"       => list_commands(config),
        "list_services"       => list_services().await,
        "get_service_logs"    => service_logs(args).await,
        "run_tests"           => run_tests(args).await,
        "list_rebuy_specs"    => list_rebuy_specs(),
        "get_rebuy_spec"      => get_rebuy_spec(args),
        "get_rebuy_spec_public" => get_rebuy_spec_public(args),
        "get_rebuy_graphql_schema" => get_rebuy_graphql_schema(args),
        "get_graphql_info"    => get_graphql_info(args),
        "list_mcp_servers"    => mcp_list_servers(),
        "add_mcp_server"      => mcp_add_server(args),
        "remove_mcp_server"   => mcp_remove_server(args),
        "map_tool"            => mcp_map_tool(args),
        "unmap_tool"          => mcp_unmap_tool(args),
        "sync_tools"          => mcp_sync_tools(args),
        "list_tool_mappings"  => mcp_list_mappings(args),
        "list_schemas"        => schema_list_databases(),
        "add_schema"          => schema_add_database(args),
        "remove_schema"       => schema_remove_database(args),
        "list_docker_runtimes" => docker_list_runtimes(),
        "add_docker_runtime"  => docker_add_runtime(args),
        "remove_docker_runtime" => docker_remove_runtime(args),
        "list_plugins"        => plugin_list(args),
        "add_plugin"          => plugin_add(args),
        "remove_plugin"       => plugin_remove(args),
        "enable_plugin"       => plugin_enable(args),
        "disable_plugin"      => plugin_disable(args),
        "list_plugin_creds"   => plugin_creds_list(args),
        "set_plugin_cred"     => plugin_creds_set(args),
        "remove_plugin_cred"  => plugin_creds_remove(args),
        "sync_plugin_creds"   => plugin_creds_sync(args),
        "register_spec"       => spec_register(args).await,
        "refresh_spec"        => spec_refresh(args).await,
        "unregister_spec"     => spec_unregister(args),
        "list_doc_roots"           => doc_list_roots(),
        "add_doc_root"             => doc_add_root(args),
        "remove_doc_root"          => doc_remove_root(args),
        "list_doc_ignore_patterns" => doc_list_ignore_patterns(),
        "add_doc_ignore_pattern"   => doc_add_ignore_pattern(args),
        "remove_doc_ignore_pattern" => doc_remove_ignore_pattern(args),
        "resolve_library" | "get_library_docs" => {
            context7::proxy_context7(name, args, config).await
        }
        "agent_backend_status"               => agent_backend_status(),
        "agent_backend_set_mode"             => agent_backend_set_mode(args),
        "agent_backend_override"             => agent_backend_override(args),
        "agent_backend_use_server_anthropic" => agent_backend_use_server_anthropic(args),
        "agent_backend_set_api_key"          => agent_backend_set_api_key(args),
        "agent_backend_clear_api_key"        => agent_backend_clear_api_key(),
        "agent_backend_api_key_status"       => agent_backend_api_key_status(),
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
