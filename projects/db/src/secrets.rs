//! Secrets — host-level secret metadata with pluggable backends.
//!
//! The `secrets` table records `{name, backend, ref_path, description}`. For
//! the `inline` backend, the actual value lives in `settings` under the
//! `secrets.{name}` prefix (so existing `settings::secret_*` helpers and the
//! `auth.login` path stay interoperable). For external backends (op, bw,
//! keychain, …) the value is fetched on demand and `ref_path` is the vendor-
//! specific address.

use anyhow::Result;
use rusqlite::Connection;

use crate::settings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRecord {
    pub name: String,
    pub backend: String,
    pub ref_path: String,
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn list(conn: &Connection) -> Result<Vec<SecretRecord>> {
    let mut stmt = conn.prepare(
        "SELECT name, backend, ref_path, description, created_at, updated_at
         FROM secrets ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SecretRecord {
            name: row.get(0)?,
            backend: row.get(1)?,
            ref_path: row.get(2)?,
            description: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<SecretRecord>> {
    let result = conn.query_row(
        "SELECT name, backend, ref_path, description, created_at, updated_at
         FROM secrets WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(SecretRecord {
                name: row.get(0)?,
                backend: row.get(1)?,
                ref_path: row.get(2)?,
                description: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        },
    );
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Upsert a metadata row. Returns true if the row was created, false on update.
pub fn upsert(
    conn: &Connection,
    name: &str,
    backend: &str,
    ref_path: &str,
    description: Option<&str>,
) -> Result<bool> {
    let existed = conn
        .query_row(
            "SELECT 1 FROM secrets WHERE name = ?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .is_ok();
    conn.execute(
        "INSERT INTO secrets (name, backend, ref_path, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4,
                 strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                 strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(name) DO UPDATE SET
             backend     = excluded.backend,
             ref_path    = excluded.ref_path,
             description = excluded.description,
             updated_at  = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![name, backend, ref_path, description],
    )?;
    Ok(!existed)
}

/// Delete the metadata row and (for inline) any stored value. Returns true if anything was removed.
pub fn delete(conn: &Connection, name: &str) -> Result<bool> {
    // Look up the record first so we know whether to also clean the inline value.
    let record = get(conn, name)?;
    let mut removed = false;
    if let Some(r) = &record {
        let n = conn.execute(
            "DELETE FROM secrets WHERE name = ?1",
            rusqlite::params![name],
        )?;
        removed = n > 0;
        if r.backend == "inline" {
            // Best-effort: ignore "not present" since the metadata is the
            // source of truth for whether the secret existed.
            let _ = settings::secret_delete(conn, name)?;
        }
    }
    Ok(removed)
}

/// Read the inline-stored value for `name`. Returns `None` if no value is stored
/// (caller should check that the record's `backend` is `inline` before invoking).
pub fn read_inline_value(conn: &Connection, name: &str) -> Result<Option<String>> {
    settings::secret_get(conn, name)
}

/// Store the inline value for `name`. The metadata row should already exist
/// (caller `upsert`s first).
pub fn write_inline_value(conn: &Connection, name: &str, value: &str) -> Result<()> {
    settings::secret_set(conn, name, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn upsert_and_get_round_trip() {
        let conn = test_conn();
        assert!(get(&conn, "github_token").unwrap().is_none());

        let created = upsert(
            &conn,
            "github_token",
            "inline",
            "",
            Some("PAT for releases"),
        )
        .unwrap();
        assert!(created, "first insert should report created");

        let r = get(&conn, "github_token").unwrap().unwrap();
        assert_eq!(r.name, "github_token");
        assert_eq!(r.backend, "inline");
        assert_eq!(r.description.as_deref(), Some("PAT for releases"));

        // Update — created should be false now.
        let created = upsert(&conn, "github_token", "inline", "", Some("rotated")).unwrap();
        assert!(!created);
        let r = get(&conn, "github_token").unwrap().unwrap();
        assert_eq!(r.description.as_deref(), Some("rotated"));
    }

    #[test]
    fn list_returns_sorted() {
        let conn = test_conn();
        upsert(&conn, "zeta", "inline", "", None).unwrap();
        upsert(&conn, "alpha", "inline", "", None).unwrap();
        upsert(&conn, "mike", "inline", "", None).unwrap();
        let names: Vec<_> = list(&conn).unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zeta"]);
    }

    #[test]
    fn delete_removes_inline_value_too() {
        let conn = test_conn();
        upsert(&conn, "k", "inline", "", None).unwrap();
        write_inline_value(&conn, "k", "v").unwrap();
        assert_eq!(read_inline_value(&conn, "k").unwrap().as_deref(), Some("v"));

        let removed = delete(&conn, "k").unwrap();
        assert!(removed);
        assert!(get(&conn, "k").unwrap().is_none());
        assert!(read_inline_value(&conn, "k").unwrap().is_none());
    }

    #[test]
    fn delete_missing_returns_false() {
        let conn = test_conn();
        assert!(!delete(&conn, "nope").unwrap());
    }
}
