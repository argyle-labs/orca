use anyhow::{Context, Result};
use orca_utils::db::{self, McpServerRow};
use clap::Subcommand;
use std::collections::HashMap;

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List all registered MCP servers (orca.db + ~/.claude.json)
    List,
    /// Add an MCP server to orca.db
    Add {
        /// Server name
        name: String,
        /// Command to run the server
        #[arg(long)]
        command: String,
        /// Arguments to pass to the command
        #[arg(long, num_args = 0..)]
        args: Vec<String>,
        /// Environment variables in KEY=VALUE format
        #[arg(long = "env", num_args = 0..)]
        env: Vec<String>,
    },
    /// Remove an MCP server from orca.db
    Remove {
        name: String,
    },
    /// Map an orca tool name to an equivalent tool on a registered MCP server
    Map {
        /// Registered MCP server name (e.g. rebuy)
        name: String,
        /// Orca tool name (the name callers use)
        orca_tool: String,
        /// External tool name on the MCP server
        external_tool: String,
    },
    /// Remove a tool mapping
    Unmap {
        /// Orca tool name to unmap
        orca_tool: String,
    },
    /// Discover or verify tool mappings for a registered MCP server
    Sync {
        /// Server name to sync (omit with --all to sync all)
        name: Option<String>,
        #[arg(long)]
        all: bool,
        /// Confidence threshold for accepting LLM matches (0.0–1.0)
        #[arg(long, default_value_t = 0.8)]
        threshold: f64,
    },
    /// List all tool mappings for a server (or all servers)
    Mappings {
        /// Server name to list (omit for all)
        name: Option<String>,
    },
}

pub fn cmd_mcp(action: McpAction) -> Result<()> {
    match action {
        McpAction::List => {
            let conn = db::open_default()?;
            let servers = db::list_mcp_servers(&conn)?;
            if servers.is_empty() {
                println!("orca.db servers: (none)");
            } else {
                println!("orca.db servers:");
                for s in &servers {
                    println!("  {} → {} {}", s.name, s.command, s.args.join(" "));
                }
            }
            let home = std::env::var("HOME").unwrap_or_default();
            let claude_path = format!("{home}/.claude.json");
            if let Ok(raw) = std::fs::read_to_string(&claude_path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
                    if let Some(servers) = json["mcpServers"].as_object() {
                        println!("~/.claude.json servers:");
                        for name in servers.keys() {
                            println!("  {name}");
                        }
                    }
                }
            }
            Ok(())
        }
        McpAction::Add { name, command, args, env } => {
            let env_map: HashMap<String, String> = env
                .iter()
                .filter_map(|e| {
                    let mut parts = e.splitn(2, '=');
                    Some((parts.next()?.to_string(), parts.next()?.to_string()))
                })
                .collect();

            let row = McpServerRow { name: name.clone(), command, args, env: env_map, enabled: true };
            let conn = db::open_default()?;
            db::upsert_mcp_server(&conn, &row)?;
            println!("added {name} to orca.db");
            Ok(())
        }
        McpAction::Remove { name } => {
            let conn = db::open_default()?;
            let removed = db::remove_mcp_server(&conn, &name)?;
            if removed {
                println!("removed {name}");
            } else {
                println!("{name} not found in orca.db");
            }
            Ok(())
        }

        McpAction::Map { name, orca_tool, external_tool } => {
            let conn = db::open_default()?;
            let servers = db::list_mcp_servers(&conn)?;
            if !servers.iter().any(|s| s.name == name) {
                anyhow::bail!("MCP server '{name}' not found in orca.db — add it first with `orca mcp add`");
            }
            let row = db::McpToolMappingRow {
                orca_tool: orca_tool.clone(),
                mcp_name: name.clone(),
                external_tool: external_tool.clone(),
                match_type: "explicit".to_string(),
                confidence: None,
                enabled: true,
            };
            db::upsert_mcp_tool_mapping(&conn, &row)?;
            println!("mapped {orca_tool} → {name}::{external_tool}");
            Ok(())
        }

        McpAction::Unmap { orca_tool } => {
            let conn = db::open_default()?;
            let removed = db::remove_mcp_tool_mapping(&conn, &orca_tool)?;
            if removed {
                println!("unmapped {orca_tool}");
            } else {
                println!("{orca_tool} not found in mcp_tool_mappings");
            }
            Ok(())
        }

        McpAction::Sync { name, all, threshold } => {
            if !all && name.is_none() {
                anyhow::bail!("usage: orca mcp sync <name> | --all");
            }
            let conn = db::open_default()?;
            let servers = db::list_mcp_servers(&conn)?;
            let targets: Vec<&McpServerRow> = if all {
                servers.iter().collect()
            } else {
                let n = name.as_deref().expect("name.is_none() rejected above");
                let s = servers.iter().find(|s| s.name == n)
                    .ok_or_else(|| anyhow::anyhow!("server '{n}' not found"))?;
                vec![s]
            };
            for server in targets {
                println!("syncing {}...", server.name);
                match mcp_sync_server(server, threshold) {
                    Ok((added, skipped)) => println!("  {} mappings added, {} skipped", added, skipped),
                    Err(e) => println!("  error: {e}"),
                }
            }
            Ok(())
        }

        McpAction::Mappings { name } => {
            let conn = db::open_default()?;
            let rows: Vec<db::McpToolMappingRow> = if let Some(n) = &name {
                db::list_mcp_tool_mappings(&conn, n)?
            } else {
                db::all_mcp_tool_mappings(&conn)?
            };
            if rows.is_empty() {
                println!("(no mappings)");
                return Ok(());
            }
            for r in &rows {
                let conf = r.confidence.map(|c| format!(" [{:.0}%]", c * 100.0)).unwrap_or_default();
                let status = if r.enabled { "" } else { " [disabled]" };
                println!("  {} → {}::{}{}{}", r.orca_tool, r.mcp_name, r.external_tool, conf, status);
            }
            Ok(())
        }
    }
}

pub fn mcp_sync_server(
    server: &db::McpServerRow,
    _threshold: f64,
) -> anyhow::Result<(usize, usize)> {
    let conn = db::open_default()?;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(&server.command)
        .args(&server.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {}", server.command))?;

    let mut stdin = child.stdin.take().context("MCP child process missing stdin pipe")?;
    let stdout = child.stdout.take().context("MCP child process missing stdout pipe")?;
    let mut reader = BufReader::new(stdout);

    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": { "name": "orca-sync", "version": "0.1.0" } }
    });
    writeln!(stdin, "{}", init)?;
    writeln!(stdin, "{}", serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;

    let tools_req = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    writeln!(stdin, "{}", tools_req)?;
    stdin.flush()?;

    let mut external_tools: Vec<serde_json::Value> = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v["id"] == 2 {
                if let Some(arr) = v["result"]["tools"].as_array() {
                    external_tools = arr.clone();
                }
                break;
            }
        }
        line.clear();
    }
    let _ = child.kill();

    if external_tools.is_empty() {
        anyhow::bail!("no tools returned from {}", server.name);
    }

    let existing = db::list_mcp_tool_mappings(&conn, &server.name)?;
    let already_mapped: std::collections::HashSet<String> = existing.iter()
        .filter(|r| r.match_type == "explicit")
        .map(|r| r.orca_tool.clone())
        .collect();

    let mut added = 0usize;
    let mut skipped = 0usize;
    for tool in &external_tools {
        let ext_name = match tool["name"].as_str() { Some(n) => n, None => continue };
        if already_mapped.contains(ext_name) { skipped += 1; continue; }
        if let Ok(Some(_)) = db::lookup_mcp_mapping(&conn, ext_name) { skipped += 1; continue; }
        let row = db::McpToolMappingRow {
            orca_tool: ext_name.to_string(),
            mcp_name: server.name.clone(),
            external_tool: ext_name.to_string(),
            match_type: "auto_discovered".to_string(),
            confidence: Some(1.0),
            enabled: true,
        };
        db::upsert_mcp_tool_mapping(&conn, &row)?;
        added += 1;
    }
    Ok((added, skipped))
}
