use anyhow::{Context, Result};
use brain_utils::consts::APP_NAME;
use brain_utils::db::{self, PluginRow};
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;

// ── Manifest parsing ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ManifestMcp {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    token_env: Option<String>,
}

#[derive(Deserialize)]
struct ManifestPlugin {
    id: String,
    version: String,
    tier: String,
    #[serde(default)]
    context_injection: Option<String>,
    #[serde(default)]
    mcp: Option<ManifestMcp>,
    /// Maps universal command name → plugin's internal MCP tool name.
    /// e.g. search_docs = "rebuy_docs_search"
    #[serde(default)]
    commands: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Manifest {
    plugin: ManifestPlugin,
}

fn parse_manifest(path: &str) -> Result<(Manifest, String)> {
    let resolved = if path.starts_with("~/") {
        let home = std::env::var("HOME").context("no HOME env var")?;
        format!("{}{}", home, &path[1..])
    } else {
        path.to_string()
    };

    let abs = std::fs::canonicalize(&resolved)
        .with_context(|| format!("manifest not found: {resolved}"))?;

    let text = std::fs::read_to_string(&abs)
        .with_context(|| format!("failed to read {}", abs.display()))?;

    let manifest: Manifest = toml::from_str(&text)
        .with_context(|| format!("invalid orca-plugin.toml at {}", abs.display()))?;

    Ok((manifest, abs.to_string_lossy().into_owned()))
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum PluginAction {
    /// Register a plugin from its orca-plugin.toml manifest
    Add {
        /// Path to orca-plugin.toml (supports ~/)
        manifest: String,
    },
    /// List all registered plugins
    List,
    /// Unregister a plugin
    Remove {
        /// Plugin id (from orca plugin list)
        id: String,
    },
    /// Enable a disabled plugin
    Enable {
        id: String,
    },
    /// Disable a plugin without removing it
    Disable {
        id: String,
    },
}

pub fn cmd_plugin(action: PluginAction) -> Result<()> {
    let conn = db::open_default()?;
    match action {
        PluginAction::Add { manifest } => {
            let (m, abs_path) = parse_manifest(&manifest)?;
            let row = PluginRow {
                id: m.plugin.id.clone(),
                manifest_path: abs_path.clone(),
                tier: m.plugin.tier.clone(),
                mcp_command: m.plugin.mcp.as_ref().map(|mcp| mcp.command.clone()),
                mcp_args: m.plugin.mcp.as_ref().map(|mcp| mcp.args.clone()).unwrap_or_default(),
                mcp_env: m.plugin.mcp.as_ref().map(|mcp| mcp.env.clone()).unwrap_or_default(),
                mcp_token_env: m.plugin.mcp.as_ref().and_then(|mcp| mcp.token_env.clone()),
                context_injection: m
                    .plugin
                    .context_injection
                    .clone()
                    .unwrap_or_else(|| "minimal".into()),
                enabled: true,
                command_map: m.plugin.commands.clone(),
            };
            db::upsert_plugin(&conn, &row)?;
            println!(
                "registered plugin '{}' v{} ({}) from {}",
                m.plugin.id, m.plugin.version, m.plugin.tier, abs_path
            );
        }

        PluginAction::List => {
            let plugins = db::list_plugins(&conn)?;
            if plugins.is_empty() {
                println!("no plugins registered — use `{APP_NAME} plugin add <path/to/{APP_NAME}-plugin.toml>`");
                return Ok(());
            }
            println!("{:<20} {:<10} {:<10} {:<8} {}", "ID", "TIER", "CONTEXT", "COMMANDS", "MCP COMMAND");
            println!("{}", "-".repeat(78));
            for p in &plugins {
                let status = if p.enabled { "" } else { " [disabled]" };
                let mcp = p.mcp_command.as_deref().unwrap_or("—");
                let ncmds = if p.command_map.is_empty() {
                    "—".to_string()
                } else {
                    p.command_map.len().to_string()
                };
                println!(
                    "{:<20} {:<10} {:<10} {:<8} {}{}",
                    p.id, p.tier, p.context_injection, ncmds, mcp, status
                );
            }
        }

        PluginAction::Remove { id } => {
            if db::remove_plugin(&conn, &id)? {
                println!("removed plugin '{id}'");
            } else {
                println!("plugin '{id}' not found");
            }
        }

        PluginAction::Enable { id } => {
            if db::set_plugin_enabled(&conn, &id, true)? {
                println!("enabled plugin '{id}'");
            } else {
                println!("plugin '{id}' not found");
            }
        }

        PluginAction::Disable { id } => {
            if db::set_plugin_enabled(&conn, &id, false)? {
                println!("disabled plugin '{id}'");
            } else {
                println!("plugin '{id}' not found");
            }
        }
    }
    Ok(())
}
