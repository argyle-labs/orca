use anyhow::{Context, Result};
use db;
use clap::Subcommand;
use rusqlite::Connection;

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
    /// Verify each registered plugin has a token and can authenticate
    Validate {
        /// Plugin id — omit to validate all registered plugins
        plugin: Option<String>,
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

        CredsAction::Validate { plugin } => {
            let conn = db::open_default()?;
            let plugins = match plugin {
                Some(id) => {
                    let p = db::get_plugin(&conn, &id)?
                        .with_context(|| format!("plugin '{id}' not registered"))?;
                    vec![p]
                }
                None => db::list_plugins(&conn)?,
            };

            if plugins.is_empty() {
                println!("no plugins registered");
                return Ok(());
            }

            println!("{:<20} {:<8} {:<10} {}", "PLUGIN", "TOKEN", "HTTP", "DETAILS");
            println!("{}", "-".repeat(72));

            let mut any_fail = false;
            for p in &plugins {
                let url = resolve_plugin_url(p);
                // Only HTTP plugins require tokens — skip stdio/subprocess plugins.
                if url.is_none() {
                    println!("{:<20} {:<8} {:<10} stdio/subprocess — no token required", p.id, "—", "—");
                    continue;
                }
                let url = url.expect("url checked as Some above via is_none() guard");

                let (token_ok, token_note) = validate_token(&conn, &p.id);
                let (http_ok, http_note) = {
                    let tok = db::list_plugin_credentials(&conn, &p.id)
                        .ok()
                        .and_then(|rows| rows.into_iter().find(|r| r.key == "MEERKAT_TOKEN"))
                        .map(|r| r.value);
                    match tok {
                        Some(t) => ping_plugin(&url, &t),
                        None => (false, "no token — run `orca creds set`".into()),
                    }
                };

                let tok_sym = if token_ok { "✓" } else { "✗" };
                let http_sym = if http_ok { "✓" } else { "✗" };
                println!("{:<20} {:<8} {:<10} token:{} http:{}", p.id, tok_sym, http_sym, token_note, http_note);

                if !token_ok || !http_ok {
                    any_fail = true;
                }
            }

            if any_fail {
                std::process::exit(1);
            }
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

pub fn resolve_plugin_url(plugin: &db::PluginRow) -> Option<String> {
    if let Some(cmd) = &plugin.mcp_command {
        if cmd.starts_with("http://") || cmd.starts_with("https://") {
            return Some(cmd.trim_end_matches('/').to_string());
        }
    }
    for arg in &plugin.mcp_args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            return Some(arg.trim_end_matches('/').to_string());
        }
    }
    None
}

/// Returns (ok, note) — whether a MEERKAT_TOKEN credential exists for this plugin.
fn validate_token(conn: &Connection, plugin_id: &str) -> (bool, String) {
    match db::list_plugin_credentials(conn, plugin_id) {
        Ok(rows) => {
            if rows.iter().any(|r| r.key == "MEERKAT_TOKEN") {
                (true, "stored".into())
            } else {
                (false, "missing".into())
            }
        }
        Err(e) => (false, format!("db error: {e}")),
    }
}

/// Returns (ok, note) — whether the plugin's /health endpoint responds with the token.
fn ping_plugin(base_url: &str, token: &str) -> (bool, String) {
    let url = format!("{base_url}/health");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("reqwest client with only a timeout always builds");
    match client.get(&url).bearer_auth(token).send() {
        Ok(resp) if resp.status().is_success() => (true, format!("ok ({})", resp.status())),
        Ok(resp) => (false, format!("HTTP {}", resp.status())),
        Err(e) => (false, format!("unreachable: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::PluginRow;
    use std::collections::HashMap;

    fn base_plugin(id: &str) -> PluginRow {
        PluginRow {
            id: id.into(),
            manifest_path: "/tmp/manifest.toml".into(),
            tier: "personal".into(),
            mode: "orca".into(),
            mcp_command: None,
            mcp_args: vec![],
            mcp_env: HashMap::new(),
            mcp_token_env: None,
            mcp_urls: vec![],
            context_injection: "minimal".into(),
            enabled: true,
            command_map: HashMap::new(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        }
    }

    #[test]
    fn resolve_plugin_url_from_http_command() {
        let p = PluginRow { mcp_command: Some("http://localhost:8080".into()), ..base_plugin("p") };
        assert_eq!(resolve_plugin_url(&p).as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn resolve_plugin_url_from_https_command_strips_trailing_slash() {
        let p = PluginRow { mcp_command: Some("https://plugin.example.com/".into()), ..base_plugin("p") };
        assert_eq!(resolve_plugin_url(&p).as_deref(), Some("https://plugin.example.com"));
    }

    #[test]
    fn resolve_plugin_url_from_http_arg_when_command_is_binary() {
        let p = PluginRow {
            mcp_command: Some("node".into()),
            mcp_args: vec!["server.js".into(), "http://localhost:9000".into()],
            ..base_plugin("p")
        };
        assert_eq!(resolve_plugin_url(&p).as_deref(), Some("http://localhost:9000"));
    }

    #[test]
    fn resolve_plugin_url_returns_none_for_stdio_plugin() {
        let p = PluginRow {
            mcp_command: Some("node".into()),
            mcp_args: vec!["server.js".into(), "--port".into(), "3000".into()],
            ..base_plugin("p")
        };
        assert!(resolve_plugin_url(&p).is_none());
    }

    #[test]
    fn resolve_plugin_url_returns_none_when_empty() {
        assert!(resolve_plugin_url(&base_plugin("p")).is_none());
    }
}
