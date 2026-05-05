use anyhow::{Context, Result};
use orca_utils::consts::APP_NAME;
use db::{self, PluginRow};
use orca_fs::fs::expand_tilde;
use clap::Subcommand;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

// ── Manifest parsing ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct ManifestMcp {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    token_env: Option<String>,
    /// HTTP/SSE endpoints tried in priority order (public domain → LAN → tailscale).
    /// Single string `url` is a shorthand for a one-element list.
    url: Option<String>,
    #[serde(default)]
    urls: Vec<String>,
}

#[derive(Deserialize, Default)]
struct ManifestSpecs {
    /// Filesystem path (supports ~/) where this plugin's spec files live.
    dir: Option<String>,
}

#[derive(Deserialize, Default)]
struct ManifestUses {
    /// Path to the dependency's orca-plugin.toml (relative to this manifest or absolute/~/…).
    path: String,
    /// Override the instance id for this dependency. Allows the same plugin template
    /// to be used multiple times with different credentials (e.g. atlassian@rebuy vs atlassian@infra).
    /// Defaults to "{dep_plugin_id}@{parent_id}" when not specified.
    id: Option<String>,
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
    /// Other plugins this plugin extends. Dependencies are installed automatically
    /// and inherit this plugin's mode so their nav links and MCPs appear in the
    /// same workspace.
    #[serde(default, rename = "uses")]
    uses: Vec<ManifestUses>,
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

/// Public entry point: install a plugin from a manifest path.
/// `instance_id` overrides the plugin's own id (for multi-instance scenarios).
pub fn install_plugin(manifest_path: &str, instance_id: Option<&str>) -> Result<String> {
    let conn = db::open_default()?;
    install_manifest(&conn, manifest_path, instance_id, None)
}

/// Public entry point: remove a plugin and cascade-remove exclusive deps.
pub fn remove_plugin(id: &str) -> Result<bool> {
    let conn = db::open_default()?;
    let deps = db::list_plugin_deps(&conn, id)?;
    db::remove_plugin_deps(&conn, id)?;
    for dep_id in &deps {
        if !db::plugin_has_parent(&conn, dep_id)? {
            db::remove_plugin(&conn, dep_id)?;
        }
    }
    db::remove_plugin(&conn, id)
}

/// Install a single plugin manifest into the DB.
///
/// - `instance_id_override`: use this id instead of the one declared in the toml.
///   Enables multiple instances of the same plugin template (e.g. `atlassian@rebuy`
///   and `atlassian@infra`) each with their own credentials and MCP connection.
/// - `mode_override`: force this mode (parent passes its own mode to deps).
///
/// Returns the instance id that was registered.
fn install_manifest(
    conn: &rusqlite::Connection,
    manifest_path: &str,
    instance_id_override: Option<&str>,
    mode_override: Option<&str>,
) -> Result<String> {
    let (m, abs_path) = parse_manifest(manifest_path)?;
    let instance_id = instance_id_override.unwrap_or(&m.plugin.id).to_string();
    let mode = mode_override.unwrap_or(&m.plugin.mode).to_string();
    let specs_dir = m.plugin.specs.as_ref()
        .and_then(|s| s.dir.as_deref())
        .map(expand_tilde);
    let row = PluginRow {
        id: instance_id.clone(),
        manifest_path: abs_path.clone(),
        tier: m.plugin.tier.clone(),
        mcp_command: m.plugin.mcp.as_ref().map(|mcp| mcp.command.clone()).filter(|c| !c.is_empty()),
        mcp_args: m.plugin.mcp.as_ref().map(|mcp| mcp.args.clone()).unwrap_or_default(),
        mcp_env: m.plugin.mcp.as_ref().map(|mcp| mcp.env.clone()).unwrap_or_default(),
        mcp_token_env: m.plugin.mcp.as_ref().and_then(|mcp| mcp.token_env.clone()),
        mcp_urls: m.plugin.mcp.as_ref().map(|mcp| {
            // `urls` list takes precedence; `url` is a single-entry shorthand.
            if !mcp.urls.is_empty() { mcp.urls.clone() }
            else if let Some(u) = &mcp.url { vec![u.clone()] }
            else { vec![] }
        }).unwrap_or_default(),
        context_injection: m.plugin.context_injection.clone().unwrap_or_else(|| "minimal".into()),
        enabled: true,
        command_map: m.plugin.commands.clone(),
        mode: mode.clone(),
        nav_links: m.plugin.nav_links.clone(),
        search_tools: m.plugin.search_tools,
        specs_dir,
    };
    db::upsert_plugin(conn, &row)?;

    let display_id = if instance_id != m.plugin.id {
        format!("{} (as '{instance_id}')", m.plugin.id)
    } else {
        instance_id.clone()
    };
    println!(
        "registered plugin {} v{} ({}) [mode: {}] from {}",
        display_id, m.plugin.version, m.plugin.tier, mode, abs_path
    );

    // Recursively install uses, resolving paths relative to this manifest's directory.
    let manifest_dir = Path::new(&abs_path).parent().unwrap_or(Path::new("."));
    for dep in &m.plugin.uses {
        let dep_path = if dep.path.starts_with('/') || dep.path.starts_with('~') {
            dep.path.clone()
        } else {
            manifest_dir.join(&dep.path).to_string_lossy().into_owned()
        };
        // Resolve the dep's base id from its manifest to build the default scoped id.
        let dep_base_id = peek_plugin_id(&dep_path).unwrap_or_else(|_| "plugin".to_string());
        let dep_instance_id = dep.id.clone()
            .unwrap_or_else(|| format!("{dep_base_id}@{instance_id}"));
        let dep_id = install_manifest(conn, &dep_path, Some(&dep_instance_id), Some(&mode))?;
        db::add_plugin_dep(conn, &instance_id, &dep_id)?;
    }

    Ok(instance_id)
}

/// Parse a manifest just to read the plugin id, without full validation.
fn peek_plugin_id(path: &str) -> Result<String> {
    let (m, _) = parse_manifest(path)?;
    Ok(m.plugin.id)
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
            install_manifest(&conn, &manifest, None, None)?;
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
            // Remove deps that were exclusively pulled in by this parent.
            let deps = db::list_plugin_deps(&conn, &id)?;
            db::remove_plugin_deps(&conn, &id)?;
            for dep_id in &deps {
                if !db::plugin_has_parent(&conn, dep_id)? {
                    if db::remove_plugin(&conn, dep_id)? {
                        println!("removed dependency '{dep_id}'");
                    }
                }
            }
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
