//! Proxmox VE endpoint registry.
//!
//! One row per cluster reachable by the local orca node. Auth is via API token
//! (`token_id` + `token_secret`); `insecure` skips TLS verification for self-signed
//! homelab clusters.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct EndpointRow {
    pub name: String,
    pub base_url: String,
    pub token_id: String,
    pub token_secret: String,
    pub insecure: bool,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<EndpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, base_url, token_id, token_secret, insecure, enabled
         FROM proxmox_endpoints ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EndpointRow {
            name: row.get(0)?,
            base_url: row.get(1)?,
            token_id: row.get(2)?,
            token_secret: row.get(3)?,
            insecure: row.get::<_, i32>(4)? != 0,
            enabled: row.get::<_, i32>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<EndpointRow>> {
    let result = conn.query_row(
        "SELECT name, base_url, token_id, token_secret, insecure, enabled
         FROM proxmox_endpoints WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(EndpointRow {
                name: row.get(0)?,
                base_url: row.get(1)?,
                token_id: row.get(2)?,
                token_secret: row.get(3)?,
                insecure: row.get::<_, i32>(4)? != 0,
                enabled: row.get::<_, i32>(5)? != 0,
            })
        },
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert(conn: &Connection, ep: &EndpointRow) -> Result<()> {
    conn.execute(
        "INSERT INTO proxmox_endpoints (name, base_url, token_id, token_secret, insecure, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
             base_url     = excluded.base_url,
             token_id     = excluded.token_id,
             token_secret = excluded.token_secret,
             insecure     = excluded.insecure,
             enabled      = excluded.enabled",
        rusqlite::params![
            ep.name,
            ep.base_url,
            ep.token_id,
            ep.token_secret,
            ep.insecure as i32,
            ep.enabled as i32,
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM proxmox_endpoints WHERE name = ?1",
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
            name: "halvor".into(),
            base_url: "https://pve.lan:8006".into(),
            token_id: "root@pam!auto".into(),
            token_secret: "deadbeef-1111-2222-3333-444444444444".into(),
            insecure: true,
            enabled: true,
        };
        upsert(&conn, &ep).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "halvor");
        assert!(rows[0].insecure);

        let got = get(&conn, "halvor").unwrap().unwrap();
        assert_eq!(got.token_id, "root@pam!auto");

        let ep2 = EndpointRow {
            name: "halvor".into(),
            base_url: "https://new.lan:8006".into(),
            token_id: "root@pam!auto".into(),
            token_secret: "rotated-uuid".into(),
            insecure: false,
            enabled: true,
        };
        upsert(&conn, &ep2).unwrap();
        let after = get(&conn, "halvor").unwrap().unwrap();
        assert_eq!(after.base_url, "https://new.lan:8006");
        assert_eq!(after.token_secret, "rotated-uuid");
        assert!(!after.insecure);

        assert!(remove(&conn, "halvor").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(get(&conn, "halvor").unwrap().is_none());
    }
}
