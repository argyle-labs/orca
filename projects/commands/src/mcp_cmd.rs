use anyhow::Result;
use brain_utils::config::Config;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// List all registered MCP servers (brain.toml + ~/.claude.json)
    List,
    /// Add an MCP server to brain.toml
    Add {
        /// Server name (used in /api/mcp/run calls)
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
    /// Remove an MCP server from brain.toml
    Remove {
        name: String,
    },
}

pub fn cmd_mcp(config: &Config, action: McpAction) -> Result<()> {
    match action {
        McpAction::List => {
            println!("brain.toml servers:");
            for s in &config.mcp_servers {
                println!("  {} → {} {}", s.name, s.command, s.args.join(" "));
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
            let path = config.brain_toml_path();
            let mut raw = std::fs::read_to_string(&path).unwrap_or_default();

            let env_pairs: Vec<(String, String)> = env
                .iter()
                .filter_map(|e| {
                    let mut parts = e.splitn(2, '=');
                    Some((parts.next()?.to_string(), parts.next()?.to_string()))
                })
                .collect();

            let args_toml = args
                .iter()
                .map(|a| format!("{:?}", a))
                .collect::<Vec<_>>()
                .join(", ");

            let env_toml = if env_pairs.is_empty() {
                String::new()
            } else {
                let pairs = env_pairs
                    .iter()
                    .map(|(k, v)| format!("{} = {:?}", k, v))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("\nenv     = {{ {} }}", pairs)
            };

            let entry = format!(
                "\n[[mcp.servers]]\nname    = {:?}\ncommand = {:?}\nargs    = [{}]{}\n",
                name, command, args_toml, env_toml
            );
            raw.push_str(&entry);
            std::fs::write(&path, &raw)?;
            println!("added {} to brain.toml", name);
            Ok(())
        }
        McpAction::Remove { name } => {
            let path = config.brain_toml_path();
            let raw = std::fs::read_to_string(&path)?;
            let mut doc: toml::Value = toml::from_str(&raw)?;
            if let Some(servers) = doc["mcp"]["servers"].as_array_mut() {
                servers.retain(|s| s["name"].as_str() != Some(&name));
            }
            std::fs::write(&path, toml::to_string_pretty(&doc)?)?;
            println!("removed {}", name);
            Ok(())
        }
    }
}
