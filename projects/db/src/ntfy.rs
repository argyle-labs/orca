//! ntfy endpoint registry. One row per registered ntfy server+topic. The
//! ntfy plugin reads `enabled` rows at startup and registers each as a
//! backend with the `notifications` dispatcher.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct EndpointRow {
    pub name: String,
    pub base_url: String,
    pub topic: String,
    pub token: Option<String>,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<EndpointRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, base_url, topic, token, enabled
         FROM ntfy_endpoints ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(EndpointRow {
            name: row.get(0)?,
            base_url: row.get(1)?,
            topic: row.get(2)?,
            token: row.get(3)?,
            enabled: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<EndpointRow>> {
    conn.query_row(
        "SELECT name, base_url, topic, token, enabled
         FROM ntfy_endpoints WHERE name = ?1",
        rusqlite::params![name],
        |row| {
            Ok(EndpointRow {
                name: row.get(0)?,
                base_url: row.get(1)?,
                topic: row.get(2)?,
                token: row.get(3)?,
                enabled: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn upsert(conn: &Connection, ep: &EndpointRow) -> Result<()> {
    conn.execute(
        "INSERT INTO ntfy_endpoints (name, base_url, topic, token, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             base_url = excluded.base_url,
             topic    = excluded.topic,
             token    = excluded.token,
             enabled  = excluded.enabled",
        rusqlite::params![ep.name, ep.base_url, ep.topic, ep.token, ep.enabled],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM ntfy_endpoints WHERE name = ?1",
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
            base_url: "http://10.10.10.6:8080".into(),
            topic: "orca-alerts".into(),
            token: Some("tk_abc".into()),
            enabled: true,
        };
        upsert(&conn, &ep).unwrap();
        assert_eq!(list(&conn).unwrap().len(), 1);

        let got = get(&conn, "home").unwrap().unwrap();
        assert_eq!(got.topic, "orca-alerts");
        assert_eq!(got.token.as_deref(), Some("tk_abc"));

        let ep2 = EndpointRow {
            token: None,
            ..ep.clone()
        };
        upsert(&conn, &ep2).unwrap();
        assert!(get(&conn, "home").unwrap().unwrap().token.is_none());

        assert!(remove(&conn, "home").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }
}
