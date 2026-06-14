//! Installed-model registry. A `Model` is a callable LLM: provider +
//! optional endpoint + a model name. Exactly one row may be marked
//! `is_default`. Per-agent pinning lives in the `settings` table under
//! `agent.<name>.model_id`. API keys live in `secrets` under
//! `model.<id>.api_key`.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: String,
    pub endpoint: Option<String>,
    pub model_name: String,
    pub is_default: bool,
    pub enabled: bool,
    pub created_at: String,
}

pub fn list(conn: &Connection) -> Result<Vec<Model>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, endpoint, model_name, is_default, enabled, created_at
           FROM models ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Model {
            id: row.get(0)?,
            provider: row.get(1)?,
            endpoint: row.get(2)?,
            model_name: row.get(3)?,
            is_default: row.get(4)?,
            enabled: row.get(5)?,
            created_at: row.get(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Model>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, endpoint, model_name, is_default, enabled, created_at
           FROM models WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Model {
            id: row.get(0)?,
            provider: row.get(1)?,
            endpoint: row.get(2)?,
            model_name: row.get(3)?,
            is_default: row.get(4)?,
            enabled: row.get(5)?,
            created_at: row.get(6)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn exists(conn: &Connection, id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM models WHERE id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn default(conn: &Connection) -> Result<Option<Model>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider, endpoint, model_name, is_default, enabled, created_at
           FROM models WHERE is_default = 1 LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        Ok(Some(Model {
            id: row.get(0)?,
            provider: row.get(1)?,
            endpoint: row.get(2)?,
            model_name: row.get(3)?,
            is_default: row.get(4)?,
            enabled: row.get(5)?,
            created_at: row.get(6)?,
        }))
    } else {
        Ok(None)
    }
}

/// Insert a new model row. Errors if `id` exists. Honours `is_default` —
/// if true, clears any existing default first.
pub fn insert(conn: &mut Connection, m: &Model) -> Result<()> {
    let tx = conn.transaction()?;
    if m.is_default {
        tx.execute("UPDATE models SET is_default = 0 WHERE is_default = 1", [])?;
    }
    tx.execute(
        "INSERT INTO models (id, provider, endpoint, model_name, is_default, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            m.id,
            m.provider,
            m.endpoint,
            m.model_name,
            m.is_default,
            m.enabled
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Update an existing model row. Errors if `id` is unknown. `is_default
/// = true` clears any other default.
pub fn update(conn: &mut Connection, m: &Model) -> Result<()> {
    let tx = conn.transaction()?;
    if m.is_default {
        tx.execute(
            "UPDATE models SET is_default = 0 WHERE is_default = 1 AND id != ?1",
            rusqlite::params![m.id],
        )?;
    }
    let n = tx.execute(
        "UPDATE models
            SET provider   = ?2,
                endpoint   = ?3,
                model_name = ?4,
                is_default = ?5,
                enabled    = ?6
          WHERE id = ?1",
        rusqlite::params![
            m.id,
            m.provider,
            m.endpoint,
            m.model_name,
            m.is_default,
            m.enabled
        ],
    )?;
    if n == 0 {
        anyhow::bail!("model '{}' not found", m.id);
    }
    tx.commit()?;
    Ok(())
}

pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM models WHERE id = ?1", rusqlite::params![id])?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn row(id: &str, is_default: bool) -> Model {
        Model {
            id: id.into(),
            provider: "lmstudio".into(),
            endpoint: Some("http://localhost:1234".into()),
            model_name: "llama3".into(),
            is_default,
            enabled: true,
            created_at: String::new(),
        }
    }

    #[test]
    fn model_crud() {
        let mut conn = test_conn();
        assert!(list(&conn).unwrap().is_empty());

        insert(&mut conn, &row("a", false)).unwrap();
        assert!(exists(&conn, "a").unwrap());
        assert!(insert(&mut conn, &row("a", false)).is_err()); // duplicate

        insert(&mut conn, &row("b", true)).unwrap();
        let d = default(&conn).unwrap().unwrap();
        assert_eq!(d.id, "b");

        // promote a to default — clears b
        let mut a = row("a", true);
        a.model_name = "llama3:70b".into();
        update(&mut conn, &a).unwrap();
        let d2 = default(&conn).unwrap().unwrap();
        assert_eq!(d2.id, "a");
        assert_eq!(d2.model_name, "llama3:70b");
        assert!(!get(&conn, "b").unwrap().unwrap().is_default);

        assert!(remove(&conn, "a").unwrap());
        assert!(!exists(&conn, "a").unwrap());
        assert!(update(&mut conn, &row("a", false)).is_err()); // missing
    }
}
