//! Plugin data store.
//!
//! Generic encrypted KV store scoped per plugin. Plugins use this to persist
//! their own state in Orca's database instead of managing their own files.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct DataRow {
    pub plugin_id: String,
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

pub fn get(conn: &Connection, plugin_id: &str, key: &str) -> Result<Option<DataRow>> {
    let result = conn.query_row(
        "SELECT plugin_id, key, value, updated_at FROM plugin_data WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
        |row| Ok(DataRow {
            plugin_id: row.get(0)?,
            key:       row.get(1)?,
            value:     row.get(2)?,
            updated_at: row.get(3)?,
        }),
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn set(conn: &Connection, plugin_id: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO plugin_data (plugin_id, key, value, updated_at)
         VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(plugin_id, key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, key, value],
    )?;
    Ok(())
}

pub fn list(conn: &Connection, plugin_id: &str) -> Result<Vec<DataRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, key, value, updated_at FROM plugin_data WHERE plugin_id = ?1 ORDER BY key",
    )?;
    let rows = stmt.query_map(rusqlite::params![plugin_id], |row| {
        Ok(DataRow {
            plugin_id: row.get(0)?,
            key: row.get(1)?,
            value: row.get(2)?,
            updated_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn delete(conn: &Connection, plugin_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_data WHERE plugin_id = ?1 AND key = ?2",
        rusqlite::params![plugin_id, key],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn set_get_list_delete() {
        let conn = test_conn();
        assert!(get(&conn, "p", "k").unwrap().is_none());

        set(&conn, "p", "key1", "val1").unwrap();
        set(&conn, "p", "key2", "val2").unwrap();

        let found = get(&conn, "p", "key1").unwrap().unwrap();
        assert_eq!(found.value, "val1");

        // Upsert
        set(&conn, "p", "key1", "updated").unwrap();
        assert_eq!(get(&conn, "p", "key1").unwrap().unwrap().value, "updated");

        let rows = list(&conn, "p").unwrap();
        assert_eq!(rows.len(), 2);

        assert!(delete(&conn, "p", "key1").unwrap());
        assert!(!delete(&conn, "p", "key1").unwrap());
        assert_eq!(list(&conn, "p").unwrap().len(), 1);
    }
}
