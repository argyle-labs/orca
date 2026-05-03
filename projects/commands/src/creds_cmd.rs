use anyhow::{Context, Result};
use brain_utils::db;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum CredsAction {
    /// Store a credential for a plugin (prompts for value)
    Set {
        /// Plugin id (from `orca plugin list`)
        plugin: String,
        /// Credential key name (e.g. THOR_PVE_SECRET)
        key: String,
        /// Value — omit to read from stdin (safer; avoids shell history)
        #[arg(short, long)]
        value: Option<String>,
    },
    /// List credential keys stored for a plugin (values never shown)
    List {
        /// Plugin id
        plugin: String,
    },
    /// Remove a credential from Orca's store
    Remove {
        /// Plugin id
        plugin: String,
        /// Credential key name
        key: String,
    },
    /// Push all stored credentials for a plugin to its running instance
    Sync {
        /// Plugin id
        plugin: String,
    },
}

pub fn cmd_creds(action: CredsAction) -> Result<()> {
    match action {
        CredsAction::Set { plugin, key, value } => {
            let value = match value {
                Some(v) => v,
                None => {
                    eprint!("value for {key}: ");
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_line(&mut buf)
                        .context("failed to read value from stdin")?;
                    buf.trim_end().to_string()
                }
            };
            let conn = db::open_default()?;
            db::set_plugin_credential(&conn, &plugin, &key, &value)?;
            println!("stored credential '{key}' for plugin '{plugin}'");
        }

        CredsAction::List { plugin } => {
            let conn = db::open_default()?;
            let rows = db::list_plugin_credentials(&conn, &plugin)?;
            if rows.is_empty() {
                println!("no credentials stored for plugin '{plugin}'");
                return Ok(());
            }
            println!("{:<40} {:<10} {}", "KEY", "SYNCED", "UPDATED");
            println!("{}", "-".repeat(72));
            for r in &rows {
                let synced = r.synced_at.as_deref().unwrap_or("never");
                println!("{:<40} {:<10} {}", r.key, synced, r.updated_at);
            }
        }

        CredsAction::Remove { plugin, key } => {
            let conn = db::open_default()?;
            if db::delete_plugin_credential(&conn, &plugin, &key)? {
                println!("removed credential '{key}' from plugin '{plugin}'");
            } else {
                println!("credential '{key}' not found for plugin '{plugin}'");
            }
        }

        CredsAction::Sync { plugin } => {
            sync_plugin_creds(&plugin)?;
        }
    }
    Ok(())
}

/// Push all credentials for a plugin to its running HTTP instance.
/// Reads plugin URL and token from the plugins table.
pub fn sync_plugin_creds(plugin_id: &str) -> Result<()> {
    let conn = db::open_default()?;

    let creds = db::list_plugin_credentials(&conn, plugin_id)?;
    if creds.is_empty() {
        println!("no credentials to sync for plugin '{plugin_id}'");
        return Ok(());
    }

    // Resolve plugin URL — stored in mcp_args or a dedicated url field.
    // For HTTP plugins the manifest url is stored in mcp_command or mcp_args.
    // Convention: first arg that starts with "http" is the base URL.
    let plugin = db::get_plugin(&conn, plugin_id)?
        .with_context(|| format!("plugin '{plugin_id}' not registered — run `orca plugin add`"))?;

    let base_url = resolve_plugin_url(&plugin)
        .with_context(|| format!("could not determine HTTP URL for plugin '{plugin_id}'\nSet url in [plugin.mcp] of the plugin manifest."))?;

    // The bearer token for the plugin is stored as a credential under key "MEERKAT_TOKEN"
    // (or whatever the plugin uses). We look for it in the stored credentials first,
    // then in the plugin's mcp_env map.
    let bearer = creds
        .iter()
        .find(|r| r.key == "MEERKAT_TOKEN")
        .map(|r| r.value.clone())
        .or_else(|| plugin.mcp_env.get("MEERKAT_TOKEN").cloned())
        .with_context(|| format!("no MEERKAT_TOKEN found for plugin '{plugin_id}'"))?;

    let client = reqwest::blocking::Client::new();
    let mut synced = 0usize;
    let mut failed = 0usize;

    for cred in &creds {
        if cred.key == "MEERKAT_TOKEN" {
            // Don't push the auth token to itself — it's already on the host.
            continue;
        }
        let url = format!("{base_url}/creds");
        let body = serde_json::json!({"key": cred.key, "value": cred.value});
        match client
            .put(&url)
            .bearer_auth(&bearer)
            .json(&body)
            .send()
        {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 204 => {
                synced += 1;
            }
            Ok(resp) => {
                eprintln!("  failed {}: HTTP {}", cred.key, resp.status());
                failed += 1;
            }
            Err(e) => {
                eprintln!("  failed {}: {}", cred.key, e);
                failed += 1;
            }
        }
    }

    if failed == 0 {
        db::mark_plugin_credentials_synced(&conn, plugin_id)?;
        println!("synced {synced} credential(s) to plugin '{plugin_id}'");
    } else {
        println!("synced {synced}, failed {failed} — credentials NOT marked as synced");
    }

    Ok(())
}

fn resolve_plugin_url(plugin: &db::PluginRow) -> Option<String> {
    // Check mcp_command — for HTTP plugins it's the base URL.
    if let Some(cmd) = &plugin.mcp_command {
        if cmd.starts_with("http://") || cmd.starts_with("https://") {
            return Some(cmd.trim_end_matches('/').to_string());
        }
    }
    // Check mcp_args for a URL.
    for arg in &plugin.mcp_args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            return Some(arg.trim_end_matches('/').to_string());
        }
    }
    None
}
