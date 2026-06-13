//! Dockge endpoint registry.
//!
//! One row per Dockge stack-manager reachable by the local orca node.
//! Auth is via bearer token. Per [[project-colocated-api-clients]] +
//! [[feedback-collectors-live-in-plugin-crate]] this row is what
//! `dockge::Client` resolves against when a tool reaches the surface.
//! Under model B (any creds-holder may execute) this table syncs to
//! every paired peer so any of them can call dockge.* against a
//! registered endpoint.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct EndpointRow {
    pub name: String,
    pub base_url: String,
    pub token: String,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<EndpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, base_url, token, enabled
         FROM dockge_endpoints ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EndpointRow {
            name: row.get(0)?,
            base_url: row.get(1)?,
            token: row.get(2)?,
            enabled: row.get::<_, i32>(3)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<EndpointRow>> {
    conn.query_row(
        "SELECT name, base_url, token, enabled
         FROM dockge_endpoints WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(EndpointRow {
                name: row.get(0)?,
                base_url: row.get(1)?,
                token: row.get(2)?,
                enabled: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Strict insert — fails with [`rusqlite::Error::SqliteFailure`] whose
/// `extended_code` is `SQLITE_CONSTRAINT_PRIMARYKEY` if `name` already
/// exists. Use this from `dockge.create` (POST semantics — refuse to
/// silently overwrite). Use [`upsert`] from mesh sync / replication
/// where last-writer-wins is intentional.
pub fn insert(conn: &Connection, ep: &EndpointRow) -> Result<()> {
    conn.execute(
        "INSERT INTO dockge_endpoints (name, base_url, token, enabled)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![ep.name, ep.base_url, ep.token, ep.enabled],
    )?;
    Ok(())
}

/// Strict update — returns `Ok(false)` if no row matched `name`. Use
/// from `dockge.update` (PATCH semantics — must already exist).
pub fn update(conn: &Connection, ep: &EndpointRow) -> Result<bool> {
    let n = conn.execute(
        "UPDATE dockge_endpoints
            SET base_url = ?2,
                token    = ?3,
                enabled  = ?4
          WHERE name = ?1",
        rusqlite::params![ep.name, ep.base_url, ep.token, ep.enabled],
    )?;
    Ok(n > 0)
}

/// Upsert — used by non-tool callers (mesh sync, replication) where
/// last-writer-wins is intentional. The tool surface uses [`insert`]
/// and [`update`] for explicit create-vs-modify semantics per the
/// REST-verbs-for-tool-surfaces feedback rule.
pub fn upsert(conn: &Connection, ep: &EndpointRow) -> Result<()> {
    conn.execute(
        "INSERT INTO dockge_endpoints (name, base_url, token, enabled)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             base_url = excluded.base_url,
             token    = excluded.token,
             enabled  = excluded.enabled",
        rusqlite::params![ep.name, ep.base_url, ep.token, ep.enabled],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM dockge_endpoints WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn fixture(name: &str, token: &str) -> EndpointRow {
        EndpointRow {
            name: name.into(),
            base_url: "http://127.0.0.1:5001".into(),
            token: token.into(),
            enabled: true,
        }
    }

    #[test]
    fn upsert_round_trip() {
        let conn = test_conn();
        upsert(&conn, &fixture("dockge-a", "tok-a")).unwrap();
        let got = get(&conn, "dockge-a").unwrap().unwrap();
        assert_eq!(got.token, "tok-a");

        let mut ep2 = fixture("dockge-a", "tok-b");
        ep2.enabled = false;
        upsert(&conn, &ep2).unwrap();
        let after = get(&conn, "dockge-a").unwrap().unwrap();
        assert_eq!(after.token, "tok-b");
        assert!(!after.enabled);

        assert!(remove(&conn, "dockge-a").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn insert_refuses_to_overwrite_existing_row() {
        let conn = test_conn();
        insert(&conn, &fixture("dockge-a", "tok-a")).unwrap();
        let err = insert(&conn, &fixture("dockge-a", "tok-b")).unwrap_err();
        // Surface the underlying rusqlite error (UNIQUE constraint
        // violation on the PRIMARY KEY). The tool layer translates this
        // into a "name already exists" error for the operator.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("UNIQUE") || msg.contains("PRIMARY"),
            "expected PK conflict, got: {msg}"
        );
        let got = get(&conn, "dockge-a").unwrap().unwrap();
        assert_eq!(got.token, "tok-a", "row must not have been overwritten");
    }

    #[test]
    fn update_returns_false_when_row_missing() {
        let conn = test_conn();
        let changed = update(&conn, &fixture("nonexistent", "tok-x")).unwrap();
        assert!(!changed, "update on a missing row must report no change");
    }

    #[test]
    fn update_applies_to_existing_row_only() {
        let conn = test_conn();
        insert(&conn, &fixture("dockge-a", "tok-a")).unwrap();
        let mut ep = fixture("dockge-a", "tok-b");
        ep.enabled = false;
        let changed = update(&conn, &ep).unwrap();
        assert!(changed);
        let after = get(&conn, "dockge-a").unwrap().unwrap();
        assert_eq!(after.token, "tok-b");
        assert!(!after.enabled);
    }
}
