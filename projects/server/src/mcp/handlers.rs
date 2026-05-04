use anyhow::Result;
use orca_core::backend::buffer_sink;
use orca_utils::config::Config;
use serde_json::Value;

use crate::context::ProjectContext;
use crate::session::Session;

pub fn agents() -> Result<String> {
    let mut lines = vec!["Available orca agents:".to_string(), String::new()];
    for (name, desc) in orca_agents::list_embedded_agents() {
        let short: String = desc.chars().take(100).collect();
        let ellipsis = if desc.len() > 100 { "…" } else { "" };
        lines.push(format!("@{name}: {short}{ellipsis}"));
    }
    Ok(lines.join("\n"))
}

pub fn get_agent(args: &Value, config: &Config) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let prompt = orca_agents::load_agent_prompt(name, &config.agents_dir())
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;
    Ok(prompt)
}

pub async fn run(args: &Value, config: &Config) -> Result<String> {
    let agent = args["agent"].as_str().unwrap_or("wolf");
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;

    let full_prompt = if agent != "wolf" && agent != "orca" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session = Session::new_with_output(config.clone(), ctx, sink).await?;
    session.one_shot(full_prompt).await?;

    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn search_logs(args: &Value, config: &Config) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query is required"))?;

    let matches = orca_utils::log::search_logs(&config.logs_dir(), query, 20)?;
    if matches.is_empty() {
        return Ok(format!("No matches for '{query}'"));
    }

    let mut lines = vec![
        format!("Found {} match(es) for '{query}':", matches.len()),
        String::new(),
    ];
    for m in &matches {
        let session = m["session"].as_str().unwrap_or("?");
        let role = m["role"].as_str().unwrap_or("?");
        let agent = m["agent"].as_str().unwrap_or("");
        let content = m["content"].as_str().unwrap_or("");
        let preview: String = content.chars().take(200).collect();
        let flag = if m["important"].as_bool() == Some(true) {
            " ★"
        } else {
            ""
        };
        let prefix = if agent.is_empty() {
            format!("[{role}]")
        } else {
            format!("[{role}/@{agent}]")
        };
        lines.push(format!("{session} {prefix} {preview}{flag}"));
    }
    Ok(lines.join("\n"))
}

pub fn get_context(args: &Value, config: &Config) -> Result<String> {
    let project = args["project"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("project is required"))?;

    let dir = config.memory_root.join(project);
    if !dir.exists() {
        return Ok(format!("No memory found for project '{project}'"));
    }

    let mut parts = vec![format!("# Memory: {project}"), String::new()];

    let index = dir.join("MEMORY.md");
    if index.exists() {
        parts.push("## MEMORY.md".to_string());
        parts.push(std::fs::read_to_string(&index)?);
    }

    let mut files: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.extension().map(|x| x == "md").unwrap_or(false)
                && p.file_name().map(|n| n != "MEMORY.md").unwrap_or(true)
        })
        .collect();
    files.sort_by_key(|e| e.file_name());

    for f in files {
        let path = f.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        parts.push(format!("## {name}"));
        parts.push(std::fs::read_to_string(&path)?);
    }

    Ok(parts.join("\n"))
}

pub fn get_config(args: &Value, config: &Config) -> Result<String> {
    let dir = config.config_dir();

    if !dir.exists() {
        return Ok(format!("Config dir not found: {}", dir.display()));
    }

    let name = args["name"].as_str().unwrap_or("").trim();

    if name.is_empty() {
        // List available config files
        let mut names: Vec<String> = std::fs::read_dir(&dir)?
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        let list = names.join(", ");
        return Ok(format!("Available config files: {list}\n\nUse orca_get_config with a name to read one."));
    }

    // Try exact match, then case-insensitive
    let candidates = [
        dir.join(format!("{name}.md")),
        dir.join(format!("{}.md", name.to_uppercase())),
    ];
    for path in &candidates {
        if path.exists() {
            return Ok(std::fs::read_to_string(path)?);
        }
    }

    // Fallback: case-insensitive scan
    let found = std::fs::read_dir(&dir)?
        .flatten()
        .find(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_uppercase() == name.to_uppercase())
                .unwrap_or(false)
        });

    match found {
        Some(e) => Ok(std::fs::read_to_string(e.path())?),
        None => Ok(format!("Config file '{name}' not found. Available: use orca_get_config with no name to list.")),
    }
}

pub async fn list_services() -> Result<String> {
    let resp = reqwest::get("http://127.0.0.1:12000/api/logs/services")
        .await?
        .json::<serde_json::Value>()
        .await?;

    let projects = resp["projects"].as_array().cloned().unwrap_or_default();
    if projects.is_empty() {
        return Ok("No projects found.".into());
    }

    let mut lines = vec![];
    for proj in &projects {
        let name = proj["project"].as_str().unwrap_or("?");
        let path = proj["path"].as_str().unwrap_or("?");
        let services = proj["services"].as_array().cloned().unwrap_or_default();
        if services.is_empty() {
            continue;
        }
        lines.push(format!("## {name}  ({path})"));
        for svc in &services {
            let svc_name = svc["name"].as_str().unwrap_or("?");
            let state = svc["state"].as_str().unwrap_or("unknown");
            let health = svc["health"].as_str().unwrap_or("");
            let ports: Vec<&str> = svc["ports"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let health_part = if health.is_empty() {
                String::new()
            } else {
                format!(" [{health}]")
            };
            let ports_part = if ports.is_empty() {
                String::new()
            } else {
                format!("  ports: {}", ports.join(", "))
            };
            lines.push(format!("  {svc_name}: {state}{health_part}{ports_part}"));
        }
    }

    Ok(lines.join("\n"))
}

pub async fn service_logs(args: &Value) -> Result<String> {
    let project = args["project"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("project is required"))?;
    let service = args["service"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("service is required"))?;
    let tail = args["tail"].as_u64().unwrap_or(200);

    let tail_str = tail.to_string();
    let resp = reqwest::Client::new()
        .get("http://127.0.0.1:12000/api/logs")
        .query(&[
            ("project", project),
            ("service", service),
            ("tail", tail_str.as_str()),
        ])
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    Ok(resp["output"].as_str().unwrap_or("(no output)").to_string())
}

pub async fn run_tests(args: &Value) -> Result<String> {
    let suite = args["suite"].as_str().unwrap_or("rust");
    let resp = crate::serve::api::run_test_suite(suite).await?;
    Ok(format!(
        "Suite: {}\nPassed: {} | Failed: {}\nDuration: {}ms\n\n{}",
        resp.suite, resp.passed, resp.failed, resp.duration_ms, resp.output
    ))
}

pub fn mcp_list_servers() -> Result<String> {
    let conn = orca_utils::db::open_default()?;
    let servers = orca_utils::db::list_mcp_servers(&conn)?;
    if servers.is_empty() {
        return Ok("No MCP servers registered in orca.db.".to_string());
    }
    let mut lines = vec!["Registered MCP servers:".to_string(), String::new()];
    for s in &servers {
        lines.push(format!("  {} → {} {}", s.name, s.command, s.args.join(" ")));
    }
    Ok(lines.join("\n"))
}

pub fn mcp_add_server(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let command = args["command"].as_str().ok_or_else(|| anyhow::anyhow!("command required"))?;
    let mcp_args: Vec<String> = args["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let env: std::collections::HashMap<String, String> = args["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let row = orca_utils::db::McpServerRow {
        name: name.to_string(),
        command: command.to_string(),
        args: mcp_args,
        env,
        enabled: true,
    };
    let conn = orca_utils::db::open_default()?;
    orca_utils::db::upsert_mcp_server(&conn, &row)?;
    Ok(format!("Registered MCP server '{name}' in orca.db."))
}

pub fn mcp_remove_server(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_mcp_server(&conn, name)? {
        Ok(format!("Removed MCP server '{name}' from orca.db."))
    } else {
        Ok(format!("Server '{name}' not found in orca.db."))
    }
}

pub fn mcp_map_tool(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let orca_tool = args["orca_tool"].as_str().ok_or_else(|| anyhow::anyhow!("orca_tool required"))?;
    let external_tool = args["external_tool"].as_str().ok_or_else(|| anyhow::anyhow!("external_tool required"))?;
    let conn = orca_utils::db::open_default()?;
    let servers = orca_utils::db::list_mcp_servers(&conn)?;
    if !servers.iter().any(|s| s.name == name) {
        anyhow::bail!("MCP server '{name}' not found in orca.db — register it first with orca_mcp_add");
    }
    let row = orca_utils::db::McpToolMappingRow {
        orca_tool: orca_tool.to_string(),
        mcp_name: name.to_string(),
        external_tool: external_tool.to_string(),
        match_type: "explicit".to_string(),
        confidence: None,
        enabled: true,
    };
    orca_utils::db::upsert_mcp_tool_mapping(&conn, &row)?;
    Ok(format!("Mapped {orca_tool} → {name}::{external_tool}"))
}

pub fn mcp_unmap_tool(args: &Value) -> Result<String> {
    let orca_tool = args["orca_tool"].as_str().ok_or_else(|| anyhow::anyhow!("orca_tool required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_mcp_tool_mapping(&conn, orca_tool)? {
        Ok(format!("Unmapped {orca_tool}"))
    } else {
        Ok(format!("{orca_tool} not found in mcp_tool_mappings"))
    }
}

pub fn mcp_sync_tools(args: &Value) -> Result<String> {
    let all = args["all"].as_bool().unwrap_or(false);
    let name = args["name"].as_str();
    let threshold = args["threshold"].as_f64().unwrap_or(0.8);
    if !all && name.is_none() {
        anyhow::bail!("provide name or set all=true");
    }
    let conn = orca_utils::db::open_default()?;
    let servers = orca_utils::db::list_mcp_servers(&conn)?;
    let targets: Vec<&orca_utils::db::McpServerRow> = if all {
        servers.iter().collect()
    } else {
        let n = name.expect("name checked above via !all && name.is_none() guard");
        let s = servers.iter().find(|s| s.name == n)
            .ok_or_else(|| anyhow::anyhow!("server '{n}' not found"))?;
        vec![s]
    };
    let mut lines = Vec::new();
    for server in targets {
        match orca_commands::mcp_sync_server(server, threshold) {
            Ok((added, skipped)) => lines.push(format!("{}: {} added, {} skipped", server.name, added, skipped)),
            Err(e) => lines.push(format!("{}: error — {e}", server.name)),
        }
    }
    Ok(lines.join("\n"))
}

pub fn mcp_list_mappings(args: &Value) -> Result<String> {
    let name = args["name"].as_str();
    let conn = orca_utils::db::open_default()?;
    let rows: Vec<orca_utils::db::McpToolMappingRow> = if let Some(n) = name {
        orca_utils::db::list_mcp_tool_mappings(&conn, n)?
    } else {
        orca_utils::db::all_mcp_tool_mappings(&conn)?
    };
    if rows.is_empty() {
        return Ok("(no mappings)".to_string());
    }
    let mut lines = Vec::new();
    for r in &rows {
        let conf = r.confidence.map(|c| format!(" [{:.0}%]", c * 100.0)).unwrap_or_default();
        let status = if r.enabled { "" } else { " [disabled]" };
        lines.push(format!("  {} → {}::{}{}{}", r.orca_tool, r.mcp_name, r.external_tool, conf, status));
    }
    Ok(lines.join("\n"))
}

pub fn schema_list_databases() -> Result<String> {
    let conn = orca_utils::db::open_default()?;
    let dbs = orca_utils::db::list_schema_databases(&conn)?;
    if dbs.is_empty() {
        return Ok("No schema databases registered. Use `orca schema add` or orca_schema_add to register one.".to_string());
    }
    let mut lines = vec!["Registered schema databases:".to_string(), String::new()];
    for d in &dbs {
        let conn_info = match (&d.container, &d.host) {
            (Some(c), _) => format!("container:{c}"),
            (None, Some(h)) => format!("{h}:{}", d.port.unwrap_or(3306)),
            _ => "unknown".to_string(),
        };
        lines.push(format!("  {} → {} @ {}", d.name, d.database, conn_info));
    }
    Ok(lines.join("\n"))
}

pub fn schema_add_database(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let database = args["database"].as_str().ok_or_else(|| anyhow::anyhow!("database required"))?;
    let user = args["user"].as_str().ok_or_else(|| anyhow::anyhow!("user required"))?;
    let password = args["password"].as_str().ok_or_else(|| anyhow::anyhow!("password required"))?;
    let row = orca_utils::db::SchemaDbRow {
        name: name.to_string(),
        driver: args["driver"].as_str().unwrap_or("mysql").to_string(),
        host: args["host"].as_str().map(|s| s.to_string()),
        port: args["port"].as_u64().map(|p| p as u16),
        user: user.to_string(),
        password: password.to_string(),
        database: database.to_string(),
        container: args["container"].as_str().map(|s| s.to_string()),
        domains_file: args["domainsFile"].as_str().map(|s| s.to_string()),
        enabled: true,
    };
    let conn = orca_utils::db::open_default()?;
    orca_utils::db::upsert_schema_database(&conn, &row)?;
    Ok(format!("Registered schema database '{name}' in orca.db."))
}

pub fn schema_remove_database(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_schema_database(&conn, name)? {
        Ok(format!("Removed schema database '{name}' from orca.db."))
    } else {
        Ok(format!("Database '{name}' not found in orca.db."))
    }
}

pub fn docker_list_runtimes() -> Result<String> {
    let conn = orca_utils::db::open_default()?;
    let rts = orca_utils::db::list_docker_runtimes(&conn)?;
    if rts.is_empty() {
        return Ok("No Docker runtimes registered. Use `orca docker add` or orca_docker_add to register one.".to_string());
    }
    let mut lines = vec!["Registered Docker runtimes:".to_string(), String::new()];
    for r in &rts {
        let target = r.docker_host()
            .or_else(|| r.url.clone())
            .unwrap_or_else(|| "(no connection)".to_string());
        let flag = if r.enabled { " [enabled]" } else { " [disabled]" };
        lines.push(format!("  {}{} → {}", r.name, flag, target));
    }
    Ok(lines.join("\n"))
}

pub fn docker_add_runtime(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let socket_path = args["socketPath"].as_str().map(|s| s.to_string());
    let host = args["host"].as_str().map(|s| s.to_string());
    let url = args["url"].as_str().map(|s| s.to_string());
    if socket_path.is_none() && host.is_none() && url.is_none() {
        anyhow::bail!("provide socketPath, host, or url");
    }
    let row = orca_utils::db::DockerRuntimeRow {
        name: name.to_string(),
        socket_path,
        host,
        url,
        enabled: true,
    };
    let conn = orca_utils::db::open_default()?;
    orca_utils::db::upsert_docker_runtime(&conn, &row)?;
    Ok(format!("Registered Docker runtime '{name}' in orca.db."))
}

pub fn docker_remove_runtime(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_docker_runtime(&conn, name)? {
        Ok(format!("Removed Docker runtime '{name}' from orca.db."))
    } else {
        Ok(format!("Runtime '{name}' not found in orca.db."))
    }
}

pub fn plugin_list(args: &Value) -> Result<String> {
    let workspace = args["workspace"].as_str();
    let conn = orca_utils::db::open_default()?;
    let plugins = orca_utils::db::list_plugins(&conn)?;
    let filtered: Vec<_> = plugins.iter()
        .filter(|p| workspace.map_or(true, |w| p.tier == w))
        .collect();
    if filtered.is_empty() {
        return Ok("No plugins registered.".to_string());
    }
    let mut lines = vec!["Registered plugins:".to_string(), String::new()];
    for p in &filtered {
        let status = if p.enabled { "enabled" } else { "disabled" };
        let cmd = p.mcp_command.as_deref().unwrap_or("(stdio)");
        lines.push(format!("  {} [{}] ({}) → {}", p.id, p.tier, status, cmd));
    }
    Ok(lines.join("\n"))
}

pub fn plugin_creds_list(args: &Value) -> Result<String> {
    let plugin = args["plugin"].as_str().ok_or_else(|| anyhow::anyhow!("plugin required"))?;
    let conn = orca_utils::db::open_default()?;
    let creds = orca_utils::db::list_plugin_credentials(&conn, plugin)?;
    if creds.is_empty() {
        return Ok(format!("No credentials stored for plugin '{plugin}'."));
    }
    let mut lines = vec![format!("Credentials for '{plugin}':")];
    for c in &creds {
        let sync = c.synced_at.as_deref().unwrap_or("never");
        lines.push(format!("  {} (synced: {}, updated: {})", c.key, sync, c.updated_at));
    }
    Ok(lines.join("\n"))
}

pub fn plugin_creds_set(args: &Value) -> Result<String> {
    let plugin = args["plugin"].as_str().ok_or_else(|| anyhow::anyhow!("plugin required"))?;
    let key = args["key"].as_str().ok_or_else(|| anyhow::anyhow!("key required"))?;
    let value = args["value"].as_str().ok_or_else(|| anyhow::anyhow!("value required"))?;
    let conn = orca_utils::db::open_default()?;
    orca_utils::db::set_plugin_credential(&conn, plugin, key, value)?;
    Ok(format!("Stored credential '{key}' for plugin '{plugin}'."))
}

pub fn plugin_creds_remove(args: &Value) -> Result<String> {
    let plugin = args["plugin"].as_str().ok_or_else(|| anyhow::anyhow!("plugin required"))?;
    let key = args["key"].as_str().ok_or_else(|| anyhow::anyhow!("key required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::delete_plugin_credential(&conn, plugin, key)? {
        Ok(format!("Removed credential '{key}' from plugin '{plugin}'."))
    } else {
        Ok(format!("Credential '{key}' not found for plugin '{plugin}'."))
    }
}

pub fn plugin_creds_sync(args: &Value) -> Result<String> {
    let plugin = args["plugin"].as_str().ok_or_else(|| anyhow::anyhow!("plugin required"))?;
    orca_commands::creds_cmd::sync_plugin_creds(plugin)?;
    Ok(format!("Synced credentials for plugin '{plugin}'."))
}

pub fn plugin_add(args: &Value) -> Result<String> {
    let manifest = args["manifest"].as_str().ok_or_else(|| anyhow::anyhow!("manifest required"))?;
    let instance_id = args["instance_id"].as_str();
    let id = orca_commands::install_plugin(manifest, instance_id)?;
    Ok(format!("Plugin '{id}' installed successfully."))
}

pub fn plugin_remove(args: &Value) -> Result<String> {
    let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
    if orca_commands::remove_plugin(id)? {
        Ok(format!("Plugin '{id}' removed."))
    } else {
        Ok(format!("Plugin '{id}' not found."))
    }
}

pub fn plugin_enable(args: &Value) -> Result<String> {
    let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::set_plugin_enabled(&conn, id, true)? {
        Ok(format!("Plugin '{id}' enabled."))
    } else {
        Ok(format!("Plugin '{id}' not found."))
    }
}

pub fn plugin_disable(args: &Value) -> Result<String> {
    let id = args["id"].as_str().ok_or_else(|| anyhow::anyhow!("id required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::set_plugin_enabled(&conn, id, false)? {
        Ok(format!("Plugin '{id}' disabled."))
    } else {
        Ok(format!("Plugin '{id}' not found."))
    }
}

// ── Doc root registry ─────────────────────────────────────────────────────────

pub fn doc_list_roots() -> Result<String> {
    let conn = orca_utils::db::open_default()?;
    let roots = orca_utils::db::list_doc_roots(&conn)?;
    if roots.is_empty() {
        return Ok("No doc roots registered. Use add_doc_root to add one.".to_string());
    }
    let mut lines = vec!["Registered doc roots:".to_string(), String::new()];
    for r in &roots {
        let desc = r.description.as_deref().unwrap_or("");
        lines.push(format!("  {} → {}  {}", r.name, r.path, desc));
    }
    Ok(lines.join("\n"))
}

pub fn doc_add_root(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let path = args["path"].as_str().ok_or_else(|| anyhow::anyhow!("path required"))?;
    let description = args["description"].as_str().map(|s| s.to_string());
    let row = orca_utils::db::DocRootRow {
        name: name.to_string(),
        path: path.to_string(),
        description,
        enabled: true,
    };
    let conn = orca_utils::db::open_default()?;
    orca_utils::db::upsert_doc_root(&conn, &row)?;
    Ok(format!("Registered doc root '{name}' → {path}"))
}

pub fn doc_remove_root(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_doc_root(&conn, name)? {
        Ok(format!("Removed doc root '{name}'."))
    } else {
        Ok(format!("Doc root '{name}' not found."))
    }
}

// ── Doc ignore patterns ───────────────────────────────────────────────────────

pub fn doc_list_ignore_patterns() -> Result<String> {
    let conn = orca_utils::db::open_default()?;
    let patterns = orca_utils::db::list_doc_ignore_patterns(&conn)?;
    if patterns.is_empty() {
        return Ok("No ignore patterns registered.".to_string());
    }
    let mut lines = vec!["Doc ignore patterns (applied to all roots):".to_string(), String::new()];
    for p in &patterns {
        lines.push(format!("  {p}"));
    }
    Ok(lines.join("\n"))
}

pub fn doc_add_ignore_pattern(args: &Value) -> Result<String> {
    let pattern = args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("pattern required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::add_doc_ignore_pattern(&conn, pattern)? {
        Ok(format!("Added ignore pattern '{pattern}'."))
    } else {
        Ok(format!("Pattern '{pattern}' already exists."))
    }
}

pub fn doc_remove_ignore_pattern(args: &Value) -> Result<String> {
    let pattern = args["pattern"].as_str().ok_or_else(|| anyhow::anyhow!("pattern required"))?;
    let conn = orca_utils::db::open_default()?;
    if orca_utils::db::remove_doc_ignore_pattern(&conn, pattern)? {
        Ok(format!("Removed ignore pattern '{pattern}'."))
    } else {
        Ok(format!("Pattern '{pattern}' not found."))
    }
}
