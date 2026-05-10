//! Home Assistant endpoint registry.
//!
//! One row per registered Home Assistant instance. The token is a long-lived
//! access token used as a Bearer credential by `orca-homeassistant`.

use anyhow::Result;
use rusqlite::Connection;

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
         FROM homeassistant_endpoints ORDER BY name",
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
    let result = conn.query_row(
        "SELECT name, base_url, token, enabled
         FROM homeassistant_endpoints WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(EndpointRow {
                name: row.get(0)?,
                base_url: row.get(1)?,
                token: row.get(2)?,
                enabled: row.get::<_, i32>(3)? != 0,
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
        "INSERT INTO homeassistant_endpoints (name, base_url, token, enabled)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             base_url = excluded.base_url,
             token    = excluded.token,
             enabled  = excluded.enabled",
        rusqlite::params![ep.name, ep.base_url, ep.token, ep.enabled as i32],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM homeassistant_endpoints WHERE name = ?1",
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
            name: "home".into(),
            base_url: "http://homeassistant.local:8123".into(),
            token: "long-lived-token".into(),
            enabled: true,
        };
        upsert(&conn, &ep).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "home");

        let got = get(&conn, "home").unwrap().unwrap();
        assert_eq!(got.token, "long-lived-token");

        let ep2 = EndpointRow {
            name: "home".into(),
            base_url: "http://ha.lan:8123".into(),
            token: "rotated".into(),
            enabled: true,
        };
        upsert(&conn, &ep2).unwrap();
        let after = get(&conn, "home").unwrap().unwrap();
        assert_eq!(after.base_url, "http://ha.lan:8123");
        assert_eq!(after.token, "rotated");

        assert!(remove(&conn, "home").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(get(&conn, "home").unwrap().is_none());
    }
}
