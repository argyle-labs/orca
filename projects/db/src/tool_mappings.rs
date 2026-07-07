//! MCP tool mapping registry — bridges orca tool names to external MCP server tools.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct MappingRow {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    pub confidence: Option<f64>,
    pub enabled: bool,
}

pub fn list(conn: &Connection, mcp_name: &str) -> Result<Vec<MappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE mcp_name = ?1 ORDER BY orca_tool",
    )?;
    let rows = stmt.query_map(rusqlite::params![mcp_name], |row| {
        Ok(MappingRow {
            orca_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn all(conn: &Connection) -> Result<Vec<MappingRow>> {
    let mut stmt = conn.prepare(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE enabled = 1 ORDER BY orca_tool",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MappingRow {
            orca_tool: row.get(0)?,
            mcp_name: row.get(1)?,
            external_tool: row.get(2)?,
            match_type: row.get(3)?,
            confidence: row.get(4)?,
            enabled: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn lookup(conn: &Connection, orca_tool: &str) -> Result<Option<MappingRow>> {
    conn.query_row(
        "SELECT orca_tool, mcp_name, external_tool, match_type, confidence, enabled
         FROM mcp_tool_mappings WHERE orca_tool = ?1 AND enabled = 1",
        rusqlite::params![orca_tool],
        |row| {
            Ok(MappingRow {
                orca_tool: row.get(0)?,
                mcp_name: row.get(1)?,
                external_tool: row.get(2)?,
                match_type: row.get(3)?,
                confidence: row.get(4)?,
                enabled: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn upsert(conn: &Connection, row: &MappingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO mcp_tool_mappings (orca_tool, mcp_name, external_tool, match_type, confidence, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(orca_tool) DO UPDATE SET
             mcp_name      = excluded.mcp_name,
             external_tool = excluded.external_tool,
             match_type    = excluded.match_type,
             confidence    = excluded.confidence,
             enabled       = excluded.enabled",
        rusqlite::params![
            row.orca_tool, row.mcp_name, row.external_tool,
            row.match_type, row.confidence, row.enabled
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, orca_tool: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM mcp_tool_mappings WHERE orca_tool = ?1",
        rusqlite::params![orca_tool],
    )?;
    Ok(n > 0)
}

pub fn set_enabled(conn: &Connection, orca_tool: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE mcp_tool_mappings SET enabled = ?1 WHERE orca_tool = ?2",
        rusqlite::params![enabled, orca_tool],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_servers;
    use crate::testing::test_conn;

    #[test]
    fn crud() {
        let conn = test_conn();
        // Need a parent MCP server (FK constraint)
        let server = mcp_servers::ServerRow {
            name: "mcp".into(),
            command: "cmd".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        mcp_servers::upsert(&conn, &server).unwrap();

        let mapping = MappingRow {
            orca_tool: "read_file".into(),
            mcp_name: "mcp".into(),
            external_tool: "fs_read".into(),
            match_type: "explicit".into(),
            confidence: Some(0.99),
            enabled: true,
        };
        upsert(&conn, &mapping).unwrap();

        let found = lookup(&conn, "read_file").unwrap().unwrap();
        assert_eq!(found.external_tool, "fs_read");
        assert!((found.confidence.unwrap() - 0.99).abs() < 1e-9);

        let all_rows = all(&conn).unwrap();
        assert_eq!(all_rows.len(), 1);

        let by_server = list(&conn, "mcp").unwrap();
        assert_eq!(by_server.len(), 1);

        assert!(set_enabled(&conn, "read_file", false).unwrap());
        assert!(
            lookup(&conn, "read_file").unwrap().is_none(),
            "disabled should not appear"
        );

        assert!(remove(&conn, "read_file").unwrap());
        assert!(!remove(&conn, "read_file").unwrap());
    }
}
