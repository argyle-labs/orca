use crate::llm::buffer_sink;
use anyhow::Result;
use orca_utils::config::Config;
use serde_json::{Value, json};

use crate::agent_backend::{self, Resolution};
use crate::context::ProjectContext;
use crate::conversation::session::Session;

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

    let resolution = agent_backend::resolve(agent, config)?;

    match resolution {
        Resolution::Local(_) => {
            // Try LM Studio first. If anything goes wrong (server unreachable,
            // no model loaded, mid-call error) fall back to delegating to
            // Claude Code so the user's task continues instead of dying.
            match run_session(agent, &full_prompt, config, None).await {
                Ok(out) => Ok(out),
                Err(e) => {
                    tracing::warn!(
                        target: "agent_backend",
                        "local run for @{agent} failed ({e:#}); falling back to claude code"
                    );
                    delegate_envelope(agent, prompt, config)
                }
            }
        }
        Resolution::ServerClaude(m) => run_session(agent, &full_prompt, config, Some(m)).await,
        Resolution::DelegateToClaudeCode => delegate_envelope(agent, prompt, config),
    }
}

async fn run_session(
    _agent: &str,
    full_prompt: &str,
    config: &Config,
    forced_model: Option<orca_utils::config::Model>,
) -> Result<String> {
    let (sink, buf) = buffer_sink();
    let ctx = ProjectContext::default();
    let mut session =
        Session::new_with_output_and_model(config.clone(), ctx, sink, forced_model).await?;
    session.one_shot(full_prompt.to_string()).await?;
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner());
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Build the structured envelope the caller (a Claude Code session) consumes
/// to run the agent itself via `get_agent` + `Agent(general-purpose)`.
fn delegate_envelope(agent: &str, prompt: &str, config: &Config) -> Result<String> {
    let agent_prompt = crate::mcp::agent_resolve::load_agent_prompt(agent, config)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {agent}"))?;
    let envelope = json!({
        "action": "delegate_to_claude_code",
        "agent": agent,
        "agent_prompt": agent_prompt,
        "task": prompt,
    });
    Ok(serde_json::to_string_pretty(&envelope)?)
}

pub fn mcp_list_servers() -> Result<String> {
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?;
    if servers.is_empty() {
        return Ok("No MCP servers registered in orca.db.".to_string());
    }
    let mut lines = vec!["Registered MCP servers:".to_string(), String::new()];
    for s in &servers {
        lines.push(format!("  {} → {} {}", s.name, s.command, s.args.join(" ")));
    }
    Ok(lines.join("\n"))
}

pub fn mcp_map_tool(args: &Value) -> Result<String> {
    let name = args["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("name required"))?;
    let orca_tool = args["orca_tool"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("orca_tool required"))?;
    let external_tool = args["external_tool"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("external_tool required"))?;
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?;
    if !servers.iter().any(|s| s.name == name) {
        anyhow::bail!(
            "MCP server '{name}' not found in orca.db — register it first with orca_mcp_add"
        );
    }
    let row = db::tool_mappings::MappingRow {
        orca_tool: orca_tool.to_string(),
        mcp_name: name.to_string(),
        external_tool: external_tool.to_string(),
        match_type: "explicit".to_string(),
        confidence: None,
        enabled: true,
    };
    db::tool_mappings::upsert(&conn, &row)?;
    Ok(format!("Mapped {orca_tool} → {name}::{external_tool}"))
}

pub fn mcp_unmap_tool(args: &Value) -> Result<String> {
    let orca_tool = args["orca_tool"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("orca_tool required"))?;
    let conn = db::open_default()?;
    if db::tool_mappings::remove(&conn, orca_tool)? {
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
    let conn = db::open_default()?;
    let servers = db::mcp_servers::list(&conn)?;
    let targets: Vec<&db::mcp_servers::ServerRow> = if all {
        servers.iter().collect()
    } else {
        let n = name.expect("name checked above via !all && name.is_none() guard");
        let s = servers
            .iter()
            .find(|s| s.name == n)
            .ok_or_else(|| anyhow::anyhow!("server '{n}' not found"))?;
        vec![s]
    };
    let mut lines = Vec::new();
    for server in targets {
        match crate::commands::mcp_sync_server(server, threshold) {
            Ok((added, skipped)) => lines.push(format!(
                "{}: {} added, {} skipped",
                server.name, added, skipped
            )),
            Err(e) => lines.push(format!("{}: error — {e}", server.name)),
        }
    }
    Ok(lines.join("\n"))
}

pub fn mcp_list_mappings(args: &Value) -> Result<String> {
    let name = args["name"].as_str();
    let conn = db::open_default()?;
    let rows: Vec<db::tool_mappings::MappingRow> = if let Some(n) = name {
        db::tool_mappings::list(&conn, n)?
    } else {
        db::tool_mappings::all(&conn)?
    };
    if rows.is_empty() {
        return Ok("(no mappings)".to_string());
    }
    let mut lines = Vec::new();
    for r in &rows {
        let conf = r
            .confidence
            .map(|c| format!(" [{:.0}%]", c * 100.0))
            .unwrap_or_default();
        let status = if r.enabled { "" } else { " [disabled]" };
        lines.push(format!(
            "  {} → {}::{}{}{}",
            r.orca_tool, r.mcp_name, r.external_tool, conf, status
        ));
    }
    Ok(lines.join("\n"))
}

pub fn schema_list_databases() -> Result<String> {
    let conn = db::open_default()?;
    let dbs = db::schema_databases::list(&conn)?;
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

pub fn docker_list_runtimes() -> Result<String> {
    let conn = db::open_default()?;
    let rts = db::docker_runtimes::list(&conn)?;
    if rts.is_empty() {
        return Ok("No Docker runtimes registered. Use `orca docker add` or orca_docker_add to register one.".to_string());
    }
    let mut lines = vec!["Registered Docker runtimes:".to_string(), String::new()];
    for r in &rts {
        let target = r
            .docker_host()
            .or_else(|| r.url.clone())
            .unwrap_or_else(|| "(no connection)".to_string());
        let flag = if r.enabled {
            " [enabled]"
        } else {
            " [disabled]"
        };
        lines.push(format!("  {}{} → {}", r.name, flag, target));
    }
    Ok(lines.join("\n"))
}

pub fn plugin_list(args: &Value) -> Result<String> {
    let workspace = args["workspace"].as_str();
    let conn = db::open_default()?;
    let plugins = db::plugins::list(&conn)?;
    let filtered: Vec<_> = plugins
        .iter()
        .filter(|p| workspace.is_none_or(|w| p.tier == w))
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
    let plugin = args["plugin"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("plugin required"))?;
    let conn = db::open_default()?;
    let creds = db::plugin_creds::list(&conn, plugin)?;
    if creds.is_empty() {
        return Ok(format!("No credentials stored for plugin '{plugin}'."));
    }
    let mut lines = vec![format!("Credentials for '{plugin}':")];
    for c in &creds {
        let sync = c.synced_at.as_deref().unwrap_or("never");
        lines.push(format!(
            "  {} (synced: {}, updated: {})",
            c.key, sync, c.updated_at
        ));
    }
    Ok(lines.join("\n"))
}

// ── Doc root registry ─────────────────────────────────────────────────────────

pub fn doc_list_roots() -> Result<String> {
    let conn = db::open_default()?;
    let roots = db::docs::list_roots(&conn)?;
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

// ── Doc ignore patterns ───────────────────────────────────────────────────────

pub fn doc_list_ignore_patterns() -> Result<String> {
    let conn = db::open_default()?;
    let patterns = db::docs::list_ignore_patterns(&conn)?;
    if patterns.is_empty() {
        return Ok("No ignore patterns registered.".to_string());
    }
    let mut lines = vec![
        "Doc ignore patterns (applied to all roots):".to_string(),
        String::new(),
    ];
    for p in &patterns {
        lines.push(format!("  {p}"));
    }
    Ok(lines.join("\n"))
}

pub fn doc_add_ignore_pattern(args: &Value) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("pattern required"))?;
    let conn = db::open_default()?;
    if db::docs::add_ignore_pattern(&conn, pattern)? {
        Ok(format!("Added ignore pattern '{pattern}'."))
    } else {
        Ok(format!("Pattern '{pattern}' already exists."))
    }
}

pub fn doc_remove_ignore_pattern(args: &Value) -> Result<String> {
    let pattern = args["pattern"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("pattern required"))?;
    let conn = db::open_default()?;
    if db::docs::remove_ignore_pattern(&conn, pattern)? {
        Ok(format!("Removed ignore pattern '{pattern}'."))
    } else {
        Ok(format!("Pattern '{pattern}' not found."))
    }
}
