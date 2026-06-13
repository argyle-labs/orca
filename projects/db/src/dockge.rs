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

    #[test]
    fn endpoint_crud() {
        let conn = test_conn();
        let ep = EndpointRow {
            name: "freyr-dockge".into(),
            base_url: "http://10.10.10.5:5001".into(),
            token: "tok-a".into(),
            enabled: true,
        };
        upsert(&conn, &ep).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "freyr-dockge");

        let got = get(&conn, "freyr-dockge").unwrap().unwrap();
        assert_eq!(got.base_url, "http://10.10.10.5:5001");

        let ep2 = EndpointRow {
            name: "freyr-dockge".into(),
            base_url: "http://10.10.10.5:5001".into(),
            token: "tok-b".into(),
            enabled: false,
        };
        upsert(&conn, &ep2).unwrap();
        let after = get(&conn, "freyr-dockge").unwrap().unwrap();
        assert_eq!(after.token, "tok-b");
        assert!(!after.enabled);

        assert!(remove(&conn, "freyr-dockge").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }
}
