//! Schema database registry — MySQL/Postgres connections used by schema-introspection tools.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct SchemaDbRow {
    pub name: String,
    pub driver: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: String,
    pub password: String,
    pub database: String,
    pub container: Option<String>,
    pub domains_file: Option<String>,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<SchemaDbRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, host, port, user, password, database, container, domains_file, enabled,
                COALESCE(driver, 'mysql')
         FROM schema_databases WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SchemaDbRow {
            name: row.get(0)?,
            host: row.get(1)?,
            port: row.get::<_, Option<i64>>(2)?.map(|p| p as u16),
            user: row.get(3)?,
            password: row.get(4)?,
            database: row.get(5)?,
            container: row.get(6)?,
            domains_file: row.get(7)?,
            enabled: row.get(8)?,
            driver: row.get(9)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn upsert(conn: &Connection, db: &SchemaDbRow) -> Result<()> {
    conn.execute(
        "INSERT INTO schema_databases (name, host, port, user, password, database, container, domains_file, enabled, driver)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(name) DO UPDATE SET
             host         = excluded.host,
             port         = excluded.port,
             user         = excluded.user,
             password     = excluded.password,
             database     = excluded.database,
             container    = excluded.container,
             domains_file = excluded.domains_file,
             enabled      = excluded.enabled,
             driver       = excluded.driver",
        rusqlite::params![
            db.name,
            db.host,
            db.port.map(|p| p as i64),
            db.user,
            db.password,
            db.database,
            db.container,
            db.domains_file,
            db.enabled,
            db.driver,
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM schema_databases WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn crud() {
        let conn = test_conn();
        assert!(list(&conn).unwrap().is_empty());

        let db = SchemaDbRow {
            name: "mydb".into(),
            driver: "postgres".into(),
            host: Some("localhost".into()),
            port: Some(5432),
            user: "admin".into(),
            password: "secret".into(),
            database: "app".into(),
            container: None,
            domains_file: None,
            enabled: true,
        };
        upsert(&conn, &db).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "mydb");
        assert_eq!(rows[0].port, Some(5432));
        assert_eq!(rows[0].driver, "postgres");

        assert!(remove(&conn, "mydb").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }
}
