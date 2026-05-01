use anyhow::Result;
use brain_utils::config::Config;
use brain_utils::db::{self, McpServerRow};
use clap::Subcommand;
use std::collections::HashMap;

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List all registered MCP servers (brain.db + ~/.claude.json)
    List,
    /// Add an MCP server to brain.db
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
    /// Remove an MCP server from brain.db
    Remove {
        name: String,
    },
}

pub fn cmd_mcp(config: &Config, action: McpAction) -> Result<()> {
    match action {
        McpAction::List => {
            let conn = db::open(&config.db_path)?;
            let servers = db::list_mcp_servers(&conn)?;
            if servers.is_empty() {
                println!("brain.db servers: (none)");
            } else {
                println!("brain.db servers:");
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
            let conn = db::open(&config.db_path)?;
            db::upsert_mcp_server(&conn, &row)?;
            println!("added {name} to brain.db");
            Ok(())
        }
        McpAction::Remove { name } => {
            let conn = db::open(&config.db_path)?;
            let removed = db::remove_mcp_server(&conn, &name)?;
            if removed {
                println!("removed {name}");
            } else {
                println!("{name} not found in brain.db");
            }
            Ok(())
        }
    }
}
