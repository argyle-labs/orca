//! OpenAPI spec registry — cached spec JSON keyed by name, with optional source URL or MCP origin.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct OpenApiSpecRow {
    pub name: String,
    pub url: Option<String>,
    pub source_mcp: Option<String>,
    pub spec_json: Option<String>,
    pub cached_at: Option<String>,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<OpenApiSpecRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, source_mcp, spec_json, cached_at, enabled
         FROM openapi_specs ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(OpenApiSpecRow {
            name: row.get(0)?,
            url: row.get(1)?,
            source_mcp: row.get(2)?,
            spec_json: row.get(3)?,
            cached_at: row.get(4)?,
            enabled: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<OpenApiSpecRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, url, source_mcp, spec_json, cached_at, enabled
         FROM openapi_specs WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![name], |row| {
        Ok(OpenApiSpecRow {
            name: row.get(0)?,
            url: row.get(1)?,
            source_mcp: row.get(2)?,
            spec_json: row.get(3)?,
            cached_at: row.get(4)?,
            enabled: row.get(5)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn upsert(conn: &Connection, spec: &OpenApiSpecRow) -> Result<()> {
    conn.execute(
        "INSERT INTO openapi_specs (name, url, source_mcp, spec_json, cached_at, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(name) DO UPDATE SET
             url        = excluded.url,
             source_mcp = excluded.source_mcp,
             spec_json  = excluded.spec_json,
             cached_at  = excluded.cached_at,
             enabled    = excluded.enabled",
        rusqlite::params![
            spec.name,
            spec.url,
            spec.source_mcp,
            spec.spec_json,
            spec.cached_at,
            spec.enabled,
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM openapi_specs WHERE name = ?1",
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

        let spec = OpenApiSpecRow {
            name: "myapi".into(),
            url: Some("http://api.example.com/openapi.json".into()),
            source_mcp: None,
            spec_json: Some(r#"{"openapi":"3.0.0"}"#.into()),
            cached_at: Some("2026-01-01T00:00:00Z".into()),
            enabled: true,
        };
        upsert(&conn, &spec).unwrap();

        let found = get(&conn, "myapi").unwrap().unwrap();
        assert_eq!(
            found.url.as_deref(),
            Some("http://api.example.com/openapi.json")
        );
        assert!(found.spec_json.is_some());

        let list = list(&conn).unwrap();
        assert_eq!(list.len(), 1);

        assert!(remove(&conn, "myapi").unwrap());
        assert!(get(&conn, "myapi").unwrap().is_none());
    }

    #[test]
    fn get_returns_none_for_missing() {
        let conn = test_conn();
        assert!(get(&conn, "ghost").unwrap().is_none());
    }
}
