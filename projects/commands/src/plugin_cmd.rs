use anyhow::{Context, Result};
use orca_utils::consts::APP_NAME;
use orca_utils::db::{self, PluginRow};
use orca_utils::tools::fs::expand_tilde;
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

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

#[derive(Deserialize, Default)]
struct ManifestSpecs {
    /// Filesystem path (supports ~/) where this plugin's spec files live.
    dir: Option<String>,
}

#[derive(Deserialize)]
struct ManifestPlugin {
    id: String,
    version: String,
    tier: String,
    /// UI mode this plugin belongs to: "orca" (default) or a custom mode string (e.g. "rebuy").
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    context_injection: Option<String>,
    #[serde(default)]
    mcp: Option<ManifestMcp>,
    /// Maps universal command name → plugin's internal MCP tool name.
    #[serde(default)]
    commands: HashMap<String, String>,
    /// Sidebar nav links this plugin contributes: [{href, label, section?}]
    #[serde(default)]
    nav_links: Vec<serde_json::Value>,
    /// MCP tools this plugin exposes for orca's unified search (Cmd+K).
    #[serde(default)]
    search_tools: Vec<db::PluginSearchTool>,
    /// Optional directory containing spec files served with this plugin's namespace.
    #[serde(default)]
    specs: Option<ManifestSpecs>,
}

fn default_mode() -> String { "orca".to_string() }

#[derive(Deserialize)]
struct Manifest {
    plugin: ManifestPlugin,
}

fn parse_manifest(path: &str) -> Result<(Manifest, String)> {
    let resolved = expand_tilde(path);

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
    /// Get a plugin data value
    DataGet {
        /// Plugin id
        id: String,
        /// Data key
        key: String,
    },
    /// Set a plugin data value
    DataSet {
        /// Plugin id
        id: String,
        /// Data key
        key: String,
        /// Value to store
        value: String,
    },
    /// List all data entries for a plugin
    DataList {
        /// Plugin id
        id: String,
    },
    /// Delete a plugin data entry
    DataDelete {
        /// Plugin id
        id: String,
        /// Data key
        key: String,
    },
}

pub fn cmd_plugin(action: PluginAction) -> Result<()> {
    let conn = db::open_default()?;
    match action {
        PluginAction::Add { manifest } => {
            let (m, abs_path) = parse_manifest(&manifest)?;
            let specs_dir = m.plugin.specs.as_ref()
                .and_then(|s| s.dir.as_deref())
                .map(expand_tilde);
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
                mode: m.plugin.mode.clone(),
                nav_links: m.plugin.nav_links.clone(),
                search_tools: m.plugin.search_tools,
                specs_dir,
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

        PluginAction::DataGet { id, key } => {
            match db::get_plugin_data(&conn, &id, &key)? {
                Some(row) => println!("{}", row.value),
                None => println!("(not set)"),
            }
        }

        PluginAction::DataSet { id, key, value } => {
            db::set_plugin_data(&conn, &id, &key, &value)?;
            println!("set {id}/{key}");
        }

        PluginAction::DataList { id } => {
            let rows = db::list_plugin_data(&conn, &id)?;
            if rows.is_empty() {
                println!("no data for plugin '{id}'");
            } else {
                println!("{:<30} {:<24} {}", "KEY", "UPDATED", "VALUE");
                println!("{}", "-".repeat(80));
                for r in rows {
                    let preview = if r.value.len() > 40 { format!("{}…", &r.value[..40]) } else { r.value.clone() };
                    println!("{:<30} {:<24} {}", r.key, r.updated_at, preview);
                }
            }
        }

        PluginAction::DataDelete { id, key } => {
            db::delete_plugin_data(&conn, &id, &key)?;
            println!("deleted {id}/{key}");
        }
    }
    Ok(())
}
