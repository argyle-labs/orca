//! LLM provider registry — local/remote inference endpoints (LM Studio, Ollama, etc.).

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub url: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: String,
}

pub fn list(conn: &Connection) -> Result<Vec<Provider>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, kind, enabled, created_at FROM llm_providers ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Provider {
            name: row.get(0)?,
            url: row.get(1)?,
            kind: row.get(2)?,
            enabled: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert(conn: &Connection, name: &str, url: &str, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_providers (name, url, kind) VALUES (?1, ?2, ?3)
         ON CONFLICT(name) DO UPDATE SET url = excluded.url, kind = excluded.kind, enabled = 1",
        rusqlite::params![name, url, kind],
    )?;
    Ok(())
}

pub fn set_enabled(conn: &Connection, name: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE llm_providers SET enabled = ?2 WHERE name = ?1",
        rusqlite::params![name, enabled],
    )?;
    Ok(n > 0)
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM llm_providers WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn provider_crud() {
        let conn = test_conn();
        assert!(list(&conn).unwrap().is_empty());

        upsert(&conn, "local", "http://localhost:1234", "lmstudio").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "local");
        assert!(rows[0].enabled);

        // Upsert updates URL
        upsert(&conn, "local", "http://localhost:5678", "lmstudio").unwrap();
        let rows2 = list(&conn).unwrap();
        assert_eq!(rows2[0].url, "http://localhost:5678");

        assert!(set_enabled(&conn, "local", false).unwrap());
        let rows3 = list(&conn).unwrap();
        assert!(!rows3[0].enabled);

        assert!(remove(&conn, "local").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(!remove(&conn, "local").unwrap());
    }
}
