//! MCP server registry — child stdio MCP servers spawned by orca's MCP bridge.

use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

use crate::{to_json_arr, to_json_obj};

#[derive(Debug, Clone)]
pub struct ServerRow {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
}

pub fn list(conn: &Connection) -> Result<Vec<ServerRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, command, args, env, enabled FROM mcp_servers WHERE enabled = 1 ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, bool>(4)?,
        ))
    })?;
    let mut result = Vec::new();
    for r in rows {
        let (name, command, args_json, env_json, enabled) = r?;
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let env: HashMap<String, String> = serde_json::from_str(&env_json).unwrap_or_default();
        result.push(ServerRow {
            name,
            command,
            args,
            env,
            enabled,
        });
    }
    Ok(result)
}

pub fn upsert(conn: &Connection, server: &ServerRow) -> Result<()> {
    let args_json = to_json_arr(&server.args);
    let env_json = to_json_obj(&server.env);
    conn.execute(
        "INSERT INTO mcp_servers (name, command, args, env, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             command = excluded.command,
             args    = excluded.args,
             env     = excluded.env,
             enabled = excluded.enabled",
        rusqlite::params![
            server.name,
            server.command,
            args_json,
            env_json,
            server.enabled
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM mcp_servers WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn server_crud() {
        let conn = test_conn();
        assert!(list(&conn).unwrap().is_empty());

        let server = ServerRow {
            name: "test-mcp".into(),
            command: "/usr/bin/node".into(),
            args: vec!["server.js".into()],
            env: [("PORT".into(), "3000".into())].into(),
            enabled: true,
        };
        upsert(&conn, &server).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "test-mcp");
        assert_eq!(rows[0].args, vec!["server.js"]);
        assert_eq!(rows[0].env.get("PORT").map(|s| s.as_str()), Some("3000"));

        assert!(remove(&conn, "test-mcp").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(!remove(&conn, "test-mcp").unwrap());
    }

    #[test]
    fn upsert_updates_existing() {
        let conn = test_conn();
        let s = ServerRow {
            name: "s".into(),
            command: "cmd1".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        upsert(&conn, &s).unwrap();
        let s2 = ServerRow {
            name: "s".into(),
            command: "cmd2".into(),
            args: vec![],
            env: Default::default(),
            enabled: true,
        };
        upsert(&conn, &s2).unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "cmd2");
    }
}
