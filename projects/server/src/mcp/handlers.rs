use anyhow::Result;
use brain_core::backend::buffer_sink;
use brain_utils::config::Config;
use serde_json::Value;

use crate::context::ProjectContext;
use crate::session::Session;

pub fn agents() -> Result<String> {
    let mut lines = vec!["Available brain agents:".to_string(), String::new()];
    for (name, desc) in brain_agents::list_embedded_agents() {
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
    let prompt = brain_agents::load_agent_prompt(name, &config.agents_dir())
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;
    Ok(prompt)
}

pub async fn run(args: &Value, config: &Config) -> Result<String> {
    let agent = args["agent"].as_str().unwrap_or("wolf");
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;

    let full_prompt = if agent != "wolf" && agent != "brain" {
        format!("Delegate this to @{agent}: {prompt}")
    } else {
        prompt.to_string()
    };

    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session = Session::new_with_output(config.clone(), ctx, sink).await?;
    session.one_shot(full_prompt).await?;

    let bytes = buf.lock().unwrap();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn search_logs(args: &Value, config: &Config) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query is required"))?;

    let matches = brain_utils::log::search_logs(&config.logs_dir(), query, 20)?;
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
        return Ok(format!("Available config files: {list}\n\nUse brain_get_config with a name to read one."));
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
        None => Ok(format!("Config file '{name}' not found. Available: use brain_get_config with no name to list.")),
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
    let conn = brain_utils::db::open_default()?;
    let servers = brain_utils::db::list_mcp_servers(&conn)?;
    if servers.is_empty() {
        return Ok("No MCP servers registered in brain.db.".to_string());
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
    let row = brain_utils::db::McpServerRow {
        name: name.to_string(),
        command: command.to_string(),
        args: mcp_args,
        env,
        enabled: true,
    };
    let conn = brain_utils::db::open_default()?;
    brain_utils::db::upsert_mcp_server(&conn, &row)?;
    Ok(format!("Registered MCP server '{name}' in brain.db."))
}

pub fn mcp_remove_server(args: &Value) -> Result<String> {
    let name = args["name"].as_str().ok_or_else(|| anyhow::anyhow!("name required"))?;
    let conn = brain_utils::db::open_default()?;
    if brain_utils::db::remove_mcp_server(&conn, name)? {
        Ok(format!("Removed MCP server '{name}' from brain.db."))
    } else {
        Ok(format!("Server '{name}' not found in brain.db."))
    }
}
