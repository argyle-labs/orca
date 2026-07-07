//! Doc root registry + doc ignore patterns — search/index roots and global ignore globs.

use anyhow::Result;
use rusqlite::Connection;

// ── Doc root registry ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RootRow {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    pub enabled: bool,
}

pub fn list_roots(conn: &Connection) -> Result<Vec<RootRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, path, description, enabled FROM doc_roots WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RootRow {
            name: row.get(0)?,
            path: row.get(1)?,
            description: row.get(2)?,
            enabled: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert_root(conn: &Connection, root: &RootRow) -> Result<()> {
    conn.execute(
        "INSERT INTO doc_roots (name, path, description, enabled)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             path        = excluded.path,
             description = excluded.description,
             enabled     = excluded.enabled",
        rusqlite::params![root.name, root.path, root.description, root.enabled],
    )?;
    Ok(())
}

pub fn remove_root(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM doc_roots WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

// ── Doc ignore patterns ──────────────────────────────────────────────────────

pub fn list_ignore_patterns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT pattern FROM doc_ignore_patterns ORDER BY pattern")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn add_ignore_pattern(conn: &Connection, pattern: &str) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO doc_ignore_patterns (pattern) VALUES (?1)",
        rusqlite::params![pattern],
    )?;
    Ok(n > 0)
}

pub fn remove_ignore_pattern(conn: &Connection, pattern: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM doc_ignore_patterns WHERE pattern = ?1",
        rusqlite::params![pattern],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn fresh_db_has_no_seeded_roots() {
        let conn = test_conn();
        let roots = list_roots(&conn).unwrap();
        assert!(roots.is_empty(), "doc_roots ships empty, got {roots:?}");
    }

    #[test]
    fn roots_crud() {
        let conn = test_conn();
        let root = RootRow {
            name: "myproject".into(),
            path: "/home/user/myproject".into(),
            description: Some("My project".into()),
            enabled: true,
        };
        upsert_root(&conn, &root).unwrap();

        let rows = list_roots(&conn).unwrap();
        assert!(rows.iter().any(|r| r.name == "myproject"));

        assert!(remove_root(&conn, "myproject").unwrap());
        assert!(
            !list_roots(&conn)
                .unwrap()
                .iter()
                .any(|r| r.name == "myproject")
        );
        assert!(!remove_root(&conn, "myproject").unwrap());
    }

    #[test]
    fn ignore_patterns_seeded_by_migration() {
        let conn = test_conn();
        let patterns = list_ignore_patterns(&conn).unwrap();
        assert!(patterns.contains(&"node_modules".to_string()));
        assert!(patterns.contains(&".git".to_string()));
        assert!(patterns.contains(&"target".to_string()));
    }

    #[test]
    fn ignore_pattern_add_remove() {
        let conn = test_conn();
        assert!(add_ignore_pattern(&conn, "my_custom_dir").unwrap());
        assert!(
            !add_ignore_pattern(&conn, "my_custom_dir").unwrap(),
            "duplicate insert should return false"
        );

        let patterns = list_ignore_patterns(&conn).unwrap();
        assert!(patterns.contains(&"my_custom_dir".to_string()));

        assert!(remove_ignore_pattern(&conn, "my_custom_dir").unwrap());
        assert!(!remove_ignore_pattern(&conn, "my_custom_dir").unwrap());
    }
}
