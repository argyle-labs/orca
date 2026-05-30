//! Plugin credentials.
//!
//! Orca is the single source of truth for plugin credentials.
//! Values are stored encrypted at rest by SQLCipher.
//! Synced to each plugin's local encrypted store via the HTTP /creds API.

use anyhow::Result;
use rusqlite::Connection;

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
/// then mark them synced. The plugin URL and bearer token are pulled from the
/// `plugins` table (URL via [`crate::plugins::PluginRow::resolve_url`], token
/// from `PLUGIN_TOKEN` in either stored credentials or the plugin's
/// `mcp_env`). On any push failure, `synced_at` is left untouched so the next
/// sync re-attempts the unsynced rows.
///
/// This is the canonical credential-sync primitive — per
/// `project_db_sync_primitive`, sync belongs in db, not in per-domain modules.
pub fn sync(plugin_id: &str) -> Result<()> {
    use anyhow::Context;

    let conn = crate::open_default()?;

    let creds = list(&conn, plugin_id)?;
    if creds.is_empty() {
        println!("no credentials to sync for plugin '{plugin_id}'");
        return Ok(());
    }

    let plugin = crate::plugins::get(&conn, plugin_id)?
        .with_context(|| format!("plugin '{plugin_id}' not registered — run `orca plugin add`"))?;

    let base_url = plugin.resolve_url().with_context(|| {
        format!(
            "could not determine HTTP URL for plugin '{plugin_id}'\nSet url in [plugin.mcp] of the plugin manifest."
        )
    })?;

    let bearer = creds
        .iter()
        .find(|r| r.key == "PLUGIN_TOKEN")
        .map(|r| r.value.clone())
        .or_else(|| plugin.mcp_env.get("PLUGIN_TOKEN").cloned())
        .with_context(|| format!("no PLUGIN_TOKEN found for plugin '{plugin_id}'"))?;

    let client = reqwest::blocking::Client::new();
    let mut synced = 0usize;
    let mut failed = 0usize;

    for cred in &creds {
        if cred.key == "PLUGIN_TOKEN" {
            // Don't push the auth token to itself — it's already on the host.
            continue;
        }
        let url = format!("{base_url}/creds");
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
    use crate::testing::test_conn;

    #[test]
    fn set_list_delete() {
        let conn = test_conn();
        set(&conn, "rebuy", "API_KEY", "secret-val").unwrap();
        set(&conn, "rebuy", "OTHER", "other-val").unwrap();

        let creds = list(&conn, "rebuy").unwrap();
        assert_eq!(creds.len(), 2);
        assert!(
            creds
                .iter()
                .any(|c| c.key == "API_KEY" && c.value == "secret-val")
        );

        // Upsert resets synced_at
        set(&conn, "rebuy", "API_KEY", "new-val").unwrap();
        let creds2 = list(&conn, "rebuy").unwrap();
        let api = creds2.iter().find(|c| c.key == "API_KEY").unwrap();
        assert_eq!(api.value, "new-val");
        assert!(
            api.synced_at.is_none(),
            "synced_at should be reset on update"
        );

        assert!(delete(&conn, "rebuy", "API_KEY").unwrap());
        assert!(!delete(&conn, "rebuy", "API_KEY").unwrap());
        assert_eq!(list(&conn, "rebuy").unwrap().len(), 1);
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
}
