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
