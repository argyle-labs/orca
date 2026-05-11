//! CLI surface for the engine domain.
//!
//! These subcommands are thin shims that dispatch to the same `OrcaTool`
//! impls in `orca_tools::engine` that drive the MCP and REST surfaces.
//! Behaviour lives in one place; this file only handles flag parsing and
//! human-friendly output formatting.

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use orca_utils::config::Config;
use orca_utils::tool::{OrcaTool, ToolCtx};
use std::sync::Arc;

use orca_tools_def::engine::{
    AddArgs, EmptyArgs, EngineAdd, EngineDisable, EngineEnable, EngineList, EngineRemove, NameArgs,
};

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

pub async fn cmd_engines(action: EnginesAction) -> Result<()> {
    let ctx = ToolCtx::new(Arc::new(Config::load()?));

    match action {
        EnginesAction::List => {
            let providers = EngineList::run(EmptyArgs {}, &ctx).await?.0;
            if providers.is_empty() {
                println!("{}", "no LLM backends registered".dimmed());
                println!(
                    "{}",
                    "  use `orca engines add <name> <url> [lmstudio|ollama]` to add one".dimmed()
                );
                return Ok(());
            }
            for p in providers {
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
            let r = EngineAdd::run(AddArgs { name, url, kind }, &ctx).await?;
            println!("{}", r.message);
        }
        EnginesAction::Remove { name } => {
            let r = EngineRemove::run(NameArgs { name }, &ctx).await?;
            println!("{}", r.message);
        }
        EnginesAction::Enable { name } => {
            let r = EngineEnable::run(NameArgs { name }, &ctx).await?;
            println!("{}", r.message);
        }
        EnginesAction::Disable { name } => {
            let r = EngineDisable::run(NameArgs { name }, &ctx).await?;
            println!("{}", r.message);
        }
    }

    Ok(())
}
