//! Plugin credential helpers — `sync_plugin_creds` shared by the MgmtService
//! impl (orca creds sync) and the `/api/plugin/creds/sync` REST handler.
//! `resolve_plugin_url` is the canonical "where does this plugin live" helper.

use anyhow::{Context, Result};
use db;

/// Push all credentials for a plugin to its running HTTP instance.
/// Reads plugin URL and token from the plugins table.
pub fn sync_plugin_creds(plugin_id: &str) -> Result<()> {
    let conn = db::open_default()?;

    let creds = db::plugin_creds::list(&conn, plugin_id)?;
    if creds.is_empty() {
        println!("no credentials to sync for plugin '{plugin_id}'");
        return Ok(());
    }

    // Resolve plugin URL — stored in mcp_args or a dedicated url field.
    // For HTTP plugins the manifest url is stored in mcp_command or mcp_args.
    // Convention: first arg that starts with "http" is the base URL.
    let plugin = db::plugins::get(&conn, plugin_id)?
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
        match client.put(&url).bearer_auth(&bearer).json(&body).send() {
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
        db::plugin_creds::mark_synced(&conn, plugin_id)?;
        println!("synced {synced} credential(s) to plugin '{plugin_id}'");
    } else {
        println!("synced {synced}, failed {failed} — credentials NOT marked as synced");
    }

    Ok(())
}

pub fn resolve_plugin_url(plugin: &db::plugins::PluginRow) -> Option<String> {
    if let Some(cmd) = &plugin.mcp_command
        && (cmd.starts_with("http://") || cmd.starts_with("https://"))
    {
        return Some(cmd.trim_end_matches('/').to_string());
    }
    for arg in &plugin.mcp_args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            return Some(arg.trim_end_matches('/').to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::plugins::PluginRow;
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
        let p = PluginRow {
            mcp_command: Some("http://localhost:8080".into()),
            ..base_plugin("p")
        };
        assert_eq!(
            resolve_plugin_url(&p).as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn resolve_plugin_url_from_https_command_strips_trailing_slash() {
        let p = PluginRow {
            mcp_command: Some("https://plugin.example.com/".into()),
            ..base_plugin("p")
        };
        assert_eq!(
            resolve_plugin_url(&p).as_deref(),
            Some("https://plugin.example.com")
        );
    }

    #[test]
    fn resolve_plugin_url_from_http_arg_when_command_is_binary() {
        let p = PluginRow {
            mcp_command: Some("node".into()),
            mcp_args: vec!["server.js".into(), "http://localhost:9000".into()],
            ..base_plugin("p")
        };
        assert_eq!(
            resolve_plugin_url(&p).as_deref(),
            Some("http://localhost:9000")
        );
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

    fn open_test_db(dir: &std::path::Path) -> (std::path::PathBuf, rusqlite::Connection) {
        let path = dir.join("test.db");
        let conn = db::open_unencrypted(&path).unwrap();
        (path, conn)
    }

    fn install_http_plugin(conn: &rusqlite::Connection, id: &str, base_url: &str) {
        let mut row = base_plugin(id);
        row.mcp_command = Some(base_url.to_string());
        db::plugins::upsert(conn, &row).unwrap();
    }

    #[test]
    fn sync_plugin_creds_no_creds_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        install_http_plugin(&conn, "p-nocreds", "http://127.0.0.1:1");
        drop(conn);
        db::set_thread_db_path(Some(path.to_str().unwrap()));
        sync_plugin_creds("p-nocreds").unwrap();
        db::set_thread_db_path(None);
    }

    #[test]
    fn sync_plugin_creds_errors_when_plugin_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        // Plugin doesn't exist; add a stray credential so we get past empty-check.
        db::plugin_creds::set(&conn, "ghost", "API_KEY", "v").unwrap();
        drop(conn);
        db::set_thread_db_path(Some(path.to_str().unwrap()));
        let err = sync_plugin_creds("ghost").err().unwrap();
        db::set_thread_db_path(None);
        assert!(
            format!("{err:#}").contains("not registered"),
            "got: {err:#}"
        );
    }

    #[test]
    fn sync_plugin_creds_errors_without_resolvable_url() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        let mut row = base_plugin("stdio-plugin");
        row.mcp_command = Some("node".into());
        row.mcp_args = vec!["server.js".into()];
        db::plugins::upsert(&conn, &row).unwrap();
        db::plugin_creds::set(&conn, "stdio-plugin", "MEERKAT_TOKEN", "tok").unwrap();
        db::plugin_creds::set(&conn, "stdio-plugin", "API_KEY", "v").unwrap();
        drop(conn);
        db::set_thread_db_path(Some(path.to_str().unwrap()));
        let err = sync_plugin_creds("stdio-plugin").err().unwrap();
        db::set_thread_db_path(None);
        assert!(format!("{err:#}").contains("HTTP URL"), "got: {err:#}");
    }

    #[test]
    fn sync_plugin_creds_errors_without_meerkat_token() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        install_http_plugin(&conn, "p-notok", "http://127.0.0.1:1");
        db::plugin_creds::set(&conn, "p-notok", "API_KEY", "v").unwrap();
        drop(conn);
        db::set_thread_db_path(Some(path.to_str().unwrap()));
        let err = sync_plugin_creds("p-notok").err().unwrap();
        db::set_thread_db_path(None);
        assert!(format!("{err:#}").contains("MEERKAT_TOKEN"), "got: {err:#}");
    }

    #[test]
    fn sync_plugin_creds_pushes_each_and_marks_synced() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base_url, _server) = rt.block_on(async {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("PUT"))
                .and(wiremock::matchers::path("/creds"))
                .respond_with(wiremock::ResponseTemplate::new(204))
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        install_http_plugin(&conn, "p-sync", &base_url);
        db::plugin_creds::set(&conn, "p-sync", "MEERKAT_TOKEN", "secret").unwrap();
        db::plugin_creds::set(&conn, "p-sync", "API_KEY", "v1").unwrap();
        db::plugin_creds::set(&conn, "p-sync", "OTHER", "v2").unwrap();
        drop(conn);

        db::set_thread_db_path(Some(path.to_str().unwrap()));
        sync_plugin_creds("p-sync").unwrap();
        db::set_thread_db_path(None);

        // mark_synced flipped synced_at on all stored rows.
        let conn = db::open_unencrypted(&path).unwrap();
        let creds = db::plugin_creds::list(&conn, "p-sync").unwrap();
        let api = creds.iter().find(|c| c.key == "API_KEY").unwrap();
        assert!(api.synced_at.is_some(), "expected synced_at to be set");
    }

    #[test]
    fn sync_plugin_creds_token_from_mcp_env_when_not_stored() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base_url, _server) = rt.block_on(async {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("PUT"))
                .respond_with(wiremock::ResponseTemplate::new(200))
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        let mut row = base_plugin("p-envtok");
        row.mcp_command = Some(base_url);
        row.mcp_env
            .insert("MEERKAT_TOKEN".into(), "env-secret".into());
        db::plugins::upsert(&conn, &row).unwrap();
        db::plugin_creds::set(&conn, "p-envtok", "API_KEY", "v").unwrap();
        drop(conn);

        db::set_thread_db_path(Some(path.to_str().unwrap()));
        sync_plugin_creds("p-envtok").unwrap();
        db::set_thread_db_path(None);
    }

    #[test]
    fn sync_plugin_creds_reports_failure_without_mark() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (base_url, _server) = rt.block_on(async {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("PUT"))
                .respond_with(wiremock::ResponseTemplate::new(500))
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_db(dir.path());
        install_http_plugin(&conn, "p-fail", &base_url);
        db::plugin_creds::set(&conn, "p-fail", "MEERKAT_TOKEN", "tok").unwrap();
        db::plugin_creds::set(&conn, "p-fail", "API_KEY", "v").unwrap();
        drop(conn);

        db::set_thread_db_path(Some(path.to_str().unwrap()));
        sync_plugin_creds("p-fail").unwrap();
        db::set_thread_db_path(None);

        // 500 → not marked synced.
        let conn = db::open_unencrypted(&path).unwrap();
        let creds = db::plugin_creds::list(&conn, "p-fail").unwrap();
        let api = creds.iter().find(|c| c.key == "API_KEY").unwrap();
        assert!(
            api.synced_at.is_none(),
            "expected synced_at to remain None on 500"
        );
    }
}
