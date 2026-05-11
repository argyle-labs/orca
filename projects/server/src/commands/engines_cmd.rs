//! CLI surface for the engine domain.
//!
//! These subcommands are thin shims that dispatch to the same `OrcaTool`
//! impls in `crate::mcp::engine_tools` that drive the MCP and REST surfaces.
//! Behaviour lives in one place; this file only handles flag parsing and
//! human-friendly output formatting.

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use orca_utils::config::Config;
use orca_utils::tool::{OrcaTool, ToolCtx};
use std::sync::Arc;

use crate::mcp::engine_tools::{
    AddArgs, EmptyArgs, EngineAdd, EngineDisable, EngineEnable, EngineList, EngineRemove, NameArgs,
    ProviderDto,
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
            let json = EngineList::run(EmptyArgs {}, &ctx).await?;
            let providers: Vec<ProviderDto> = serde_json::from_str(&json)?;
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
            let msg = rt.block_on(EngineAdd::run(AddArgs { name, url, kind }, &ctx))?;
            println!("{msg}");
        }
        EnginesAction::Remove { name } => {
            let msg = rt.block_on(EngineRemove::run(NameArgs { name }, &ctx))?;
            println!("{msg}");
        }
        EnginesAction::Enable { name } => {
            let msg = rt.block_on(EngineEnable::run(NameArgs { name }, &ctx))?;
            println!("{msg}");
        }
        EnginesAction::Disable { name } => {
            let msg = rt.block_on(EngineDisable::run(NameArgs { name }, &ctx))?;
            println!("{msg}");
        }
    }

    Ok(())
}
