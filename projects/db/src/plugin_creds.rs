//! Plugin credentials.
//!
//! Orca is the single source of truth for plugin credentials.
//! Values are stored encrypted at rest by SQLCipher.
//! Synced to each plugin's local encrypted store via the HTTP /creds API.

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::plugin_manifest;

#[derive(Debug, Clone)]
pub struct CredentialRow {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
    pub synced_at: Option<String>,
    pub updated_at: String,
}

/// Store or update a credential for a plugin.
pub fn set(conn: &Connection, plugin_id: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO plugin_credentials (plugin_id, key, value, synced_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(plugin_id, key) DO UPDATE SET
             value      = excluded.value,
             synced_at  = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, key, value],
    )?;
    Ok(())
}

/// List all credentials for a plugin. Returns key names and metadata; value is included
/// for sync purposes — never surface values in CLI output.
pub fn list(conn: &Connection, plugin_id: &str) -> Result<Vec<CredentialRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, key, value, synced_at, updated_at
         FROM plugin_credentials WHERE plugin_id = ?1 ORDER BY key",
    )?;
    let rows = stmt.query_map(rusqlite::params![plugin_id], |row| {
        Ok(CredentialRow {
            plugin_id: row.get(0)?,
            key: row.get(1)?,
            value: row.get(2)?,
            synced_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Delete a single credential for a plugin.
pub fn delete(conn: &Connection, plugin_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_credentials WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
    )?;
    Ok(n > 0)
}

/// Mark all credentials for a plugin as synced (called after a successful push).
pub fn mark_synced(conn: &Connection, plugin_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE plugin_credentials SET synced_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE plugin_id = ?1",
        rusqlite::params![plugin_id],
    )?;
    Ok(())
}

/// Push every stored credential for `plugin_id` to its running HTTP instance,
/// then mark them synced. The plugin URL and bearer token are resolved by
/// parsing the plugin's manifest at `plugins.manifest_path`. The token comes
/// from a stored `PLUGIN_TOKEN` credential, falling back to the `mcp.env`
/// value if the credential isn't stored yet. On any push failure, `synced_at`
/// is left untouched so the next sync re-attempts the unsynced rows.
///
/// This is the canonical credential-sync primitive — per
/// `project_db_sync_primitive`, sync belongs in db, not in per-domain modules.
pub fn sync(plugin_id: &str) -> Result<()> {
    let conn = crate::open_default()?;

    let creds = list(&conn, plugin_id)?;
    if creds.is_empty() {
        println!("no credentials to sync for plugin '{plugin_id}'");
        return Ok(());
    }

    let plugin = crate::plugins::get(&conn, plugin_id)?
        .with_context(|| format!("plugin '{plugin_id}' not registered — run `orca plugin add`"))?;

    let (manifest, _) = plugin_manifest::parse_path(&plugin.manifest_path)?;

    let base_url = manifest.resolve_url().with_context(|| {
        format!(
            "could not determine HTTP URL for plugin '{plugin_id}'\nSet url in [plugin.mcp] of the plugin manifest."
        )
    })?;

    let bearer = creds
        .iter()
        .find(|r| r.key == "PLUGIN_TOKEN")
        .map(|r| r.value.clone())
        .or_else(|| {
            manifest
                .plugin
                .mcp
                .as_ref()
                .and_then(|m| m.env.get("PLUGIN_TOKEN").cloned())
        })
        .with_context(|| format!("no PLUGIN_TOKEN found for plugin '{plugin_id}'"))?;

    let client = reqwest::blocking::Client::new();
    let mut synced = 0usize;
    let mut failed = 0usize;

    for cred in &creds {
        if cred.key == "PLUGIN_TOKEN" {
            // Don't push the auth token to itself — it's already on the host.
            continue;
        }
        let url = utils::url::join(&base_url, "creds");
        #[allow(clippy::disallowed_types)]
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
        mark_synced(&conn, plugin_id)?;
        println!("synced {synced} credential(s) to plugin '{plugin_id}'");
    } else {
        println!("synced {synced}, failed {failed} — credentials NOT marked as synced");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::PluginRow;
    use crate::testing::test_conn;

    #[test]
    fn set_list_delete() {
        let conn = test_conn();
        set(&conn, "acme", "API_KEY", "secret-val").unwrap();
        set(&conn, "acme", "OTHER", "other-val").unwrap();

        let creds = list(&conn, "acme").unwrap();
        assert_eq!(creds.len(), 2);
        assert!(
            creds
                .iter()
                .any(|c| c.key == "API_KEY" && c.value == "secret-val")
        );

        // Upsert resets synced_at
        set(&conn, "acme", "API_KEY", "new-val").unwrap();
        let creds2 = list(&conn, "acme").unwrap();
        let api = creds2.iter().find(|c| c.key == "API_KEY").unwrap();
        assert_eq!(api.value, "new-val");
        assert!(
            api.synced_at.is_none(),
            "synced_at should be reset on update"
        );

        assert!(delete(&conn, "acme", "API_KEY").unwrap());
        assert!(!delete(&conn, "acme", "API_KEY").unwrap());
        assert_eq!(list(&conn, "acme").unwrap().len(), 1);
    }

    #[test]
    fn synced_at_set_after_mark() {
        let conn = test_conn();
        set(&conn, "p", "K", "V").unwrap();
        let before = list(&conn, "p").unwrap();
        assert!(before[0].synced_at.is_none());

        mark_synced(&conn, "p").unwrap();
        let after = list(&conn, "p").unwrap();
        assert!(after[0].synced_at.is_some());
    }

    // ── sync() — credential push primitive ────────────────────────────────────

    fn write_manifest(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("orca-plugin.toml");
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn http_manifest(id: &str, base_url: &str) -> String {
        format!(
            r#"
[plugin]
id = "{id}"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
url = "{base_url}"
"#
        )
    }

    fn install_http_plugin(conn: &Connection, dir: &std::path::Path, id: &str, base_url: &str) {
        let manifest_path = write_manifest(dir, &http_manifest(id, base_url));
        let row = PluginRow {
            id: id.into(),
            manifest_path,
            tier: "personal".into(),
            context_injection: "minimal".into(),
            enabled: true,
            command_map: Default::default(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        };
        crate::plugins::upsert(conn, &row).unwrap();
    }

    fn open_test_path(dir: &std::path::Path) -> (std::path::PathBuf, Connection) {
        let path = dir.join("test.db");
        let conn = crate::open_unencrypted(&path).unwrap();
        (path, conn)
    }

    #[test]
    fn sync_no_creds_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_path(dir.path());
        install_http_plugin(&conn, dir.path(), "p-nocreds", "http://127.0.0.1:1");
        drop(conn);
        crate::with_thread_db_path(&path, || sync("p-nocreds").unwrap());
    }

    #[test]
    fn sync_errors_when_plugin_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_path(dir.path());
        set(&conn, "ghost", "API_KEY", "v").unwrap();
        drop(conn);
        let err = crate::with_thread_db_path(&path, || sync("ghost").err().unwrap());
        assert!(
            format!("{err:#}").contains("not registered"),
            "got: {err:#}"
        );
    }

    #[test]
    fn sync_errors_without_resolvable_url() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_path(dir.path());
        let manifest_path = write_manifest(
            dir.path(),
            r#"
[plugin]
id = "stdio-plugin"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = "node"
args = ["server.js"]
"#,
        );
        let row = PluginRow {
            id: "stdio-plugin".into(),
            manifest_path,
            tier: "personal".into(),
            context_injection: "minimal".into(),
            enabled: true,
            command_map: Default::default(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        };
        crate::plugins::upsert(&conn, &row).unwrap();
        set(&conn, "stdio-plugin", "PLUGIN_TOKEN", "tok").unwrap();
        set(&conn, "stdio-plugin", "API_KEY", "v").unwrap();
        drop(conn);
        let err = crate::with_thread_db_path(&path, || sync("stdio-plugin").err().unwrap());
        assert!(format!("{err:#}").contains("HTTP URL"), "got: {err:#}");
    }

    /// Install rustls' ring CryptoProvider exactly once per process.
    /// `install_default()` errors when a provider is already registered, which
    /// is the *expected* path on the 2nd+ test in this module — guard with
    /// `Once` so the first install panics on real failures and later calls are
    /// no-ops (no error swallowed).
    fn install_ring_provider_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("install rustls ring CryptoProvider");
        });
    }

    #[test]
    fn sync_errors_without_plugin_token() {
        let dir = tempfile::tempdir().unwrap();
        let (path, conn) = open_test_path(dir.path());
        install_http_plugin(&conn, dir.path(), "p-notok", "http://127.0.0.1:1");
        set(&conn, "p-notok", "API_KEY", "v").unwrap();
        drop(conn);
        let err = crate::with_thread_db_path(&path, || sync("p-notok").err().unwrap());
        assert!(format!("{err:#}").contains("PLUGIN_TOKEN"), "got: {err:#}");
    }

    #[test]
    fn sync_pushes_each_and_marks_synced() {
        install_ring_provider_once();
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
        let (path, conn) = open_test_path(dir.path());
        install_http_plugin(&conn, dir.path(), "p-sync", &base_url);
        set(&conn, "p-sync", "PLUGIN_TOKEN", "secret").unwrap();
        set(&conn, "p-sync", "API_KEY", "v1").unwrap();
        set(&conn, "p-sync", "OTHER", "v2").unwrap();
        drop(conn);

        crate::with_thread_db_path(&path, || sync("p-sync").unwrap());

        let conn = crate::open_unencrypted(&path).unwrap();
        let creds = list(&conn, "p-sync").unwrap();
        let api = creds.iter().find(|c| c.key == "API_KEY").unwrap();
        assert!(api.synced_at.is_some(), "expected synced_at to be set");
    }

    #[test]
    fn sync_token_from_mcp_env_when_not_stored() {
        install_ring_provider_once();
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
        let (path, conn) = open_test_path(dir.path());
        let manifest_path = write_manifest(
            dir.path(),
            &format!(
                r#"
[plugin]
id = "p-envtok"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
url = "{base_url}"

[plugin.mcp.env]
PLUGIN_TOKEN = "env-secret"
"#
            ),
        );
        let row = PluginRow {
            id: "p-envtok".into(),
            manifest_path,
            tier: "personal".into(),
            context_injection: "minimal".into(),
            enabled: true,
            command_map: Default::default(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        };
        crate::plugins::upsert(&conn, &row).unwrap();
        set(&conn, "p-envtok", "API_KEY", "v").unwrap();
        drop(conn);

        crate::with_thread_db_path(&path, || sync("p-envtok").unwrap());
    }

    #[test]
    fn sync_reports_failure_without_mark() {
        install_ring_provider_once();
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
        let (path, conn) = open_test_path(dir.path());
        install_http_plugin(&conn, dir.path(), "p-fail", &base_url);
        set(&conn, "p-fail", "PLUGIN_TOKEN", "tok").unwrap();
        set(&conn, "p-fail", "API_KEY", "v").unwrap();
        drop(conn);

        crate::with_thread_db_path(&path, || sync("p-fail").unwrap());

        let conn = crate::open_unencrypted(&path).unwrap();
        let creds = list(&conn, "p-fail").unwrap();
        let api = creds.iter().find(|c| c.key == "API_KEY").unwrap();
        assert!(
            api.synced_at.is_none(),
            "500 response must leave synced_at as None"
        );
    }
}
