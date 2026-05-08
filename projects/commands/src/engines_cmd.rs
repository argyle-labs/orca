use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use db;

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum EnginesAction {
    /// List registered LLM backends
    List,
    /// Register an LLM backend (kind: ollama|lmstudio, defaults to auto-detect from port)
    Add {
        name: String,
        url: String,
        #[arg(default_value = "")]
        kind: String,
    },
    /// Remove a registered backend
    Remove { name: String },
    /// Enable a backend for model discovery
    Enable { name: String },
    /// Disable a backend without removing it
    Disable { name: String },
}

pub fn cmd_engines(action: EnginesAction) -> Result<()> {
    let conn = db::open_default()?;

    match action {
        EnginesAction::List => {
            let providers = db::list_llm_providers(&conn)?;
            if providers.is_empty() {
                println!("{}", "no LLM backends registered".dimmed());
                println!(
                    "{}",
                    "  use `orca engines add <name> <url> [lmstudio|ollama]` to add one".dimmed()
                );
                return Ok(());
            }
            for p in &providers {
                let status = if p.enabled {
                    "enabled".green().to_string()
                } else {
                    "disabled".dimmed().to_string()
                };
                println!(
                    "  {} {} {} ({})",
                    p.name.bold(),
                    p.kind.cyan(),
                    p.url,
                    status
                );
            }
        }

        EnginesAction::Add { name, url, kind } => {
            let kind = if kind.is_empty() {
                // Infer kind from URL: default port 11434 → ollama, else lmstudio
                if url.contains(":11434") {
                    "ollama".to_string()
                } else {
                    "lmstudio".to_string()
                }
            } else {
                kind
            };
            match kind.as_str() {
                "ollama" | "lmstudio" => {}
                other => anyhow::bail!("unknown backend kind '{other}' (want: ollama|lmstudio)"),
            }
            db::upsert_llm_provider(&conn, &name, &url, &kind)?;
            println!("registered {} {} ({})", kind.cyan(), name.bold(), url);
        }

        EnginesAction::Remove { name } => {
            if db::remove_llm_provider(&conn, &name)? {
                println!("removed {}", name.bold());
            } else {
                anyhow::bail!("no backend named '{name}'");
            }
        }

        EnginesAction::Enable { name } => {
            if db::set_llm_provider_enabled(&conn, &name, true)? {
                println!("{} enabled", name.bold());
            } else {
                anyhow::bail!("no backend named '{name}'");
            }
        }

        EnginesAction::Disable { name } => {
            if db::set_llm_provider_enabled(&conn, &name, false)? {
                println!("{} disabled", name.bold());
            } else {
                anyhow::bail!("no backend named '{name}'");
            }
        }
    }

    Ok(())
}
