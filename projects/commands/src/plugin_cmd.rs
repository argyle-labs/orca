use anyhow::{Context, Result};
use brain_utils::consts::APP_NAME;
use brain_utils::db::{self, PluginRow};
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;

// ── Manifest parsing ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ManifestPlugin {
    id: String,
    version: String,
    tier: String,
    #[serde(default)]
    context_injection: Option<String>,
}

#[derive(Deserialize, Default)]
struct ManifestMcp {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Deserialize)]
struct Manifest {
    plugin: ManifestPlugin,
    #[serde(rename = "plugin.mcp")]
    plugin_mcp: Option<ManifestMcp>,
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
        .with_context(|| format!("invalid brain-plugin.toml at {}", abs.display()))?;

    Ok((manifest, abs.to_string_lossy().into_owned()))
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum PluginAction {
    /// Register a plugin from its brain-plugin.toml manifest
    Add {
        /// Path to brain-plugin.toml (supports ~/)
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
                mcp_command: m.plugin_mcp.as_ref().map(|mcp| mcp.command.clone()),
                mcp_args: m.plugin_mcp.as_ref().map(|mcp| mcp.args.clone()).unwrap_or_default(),
                mcp_env: m.plugin_mcp.as_ref().map(|mcp| mcp.env.clone()).unwrap_or_default(),
                context_injection: m
                    .plugin
                    .context_injection
                    .clone()
                    .unwrap_or_else(|| "minimal".into()),
                enabled: true,
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
            println!("{:<20} {:<10} {:<10} {}", "ID", "TIER", "CONTEXT", "MCP COMMAND");
            println!("{}", "-".repeat(70));
            for p in &plugins {
                let status = if p.enabled { "" } else { " [disabled]" };
                let mcp = p.mcp_command.as_deref().unwrap_or("—");
                println!(
                    "{:<20} {:<10} {:<10} {}{}",
                    p.id, p.tier, p.context_injection, mcp, status
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
