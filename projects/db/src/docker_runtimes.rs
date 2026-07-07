//! Docker runtime registry — sockets, TCP hosts, and web orchestrators (Dockge, Portainer).
//!
//! `RuntimeRow::docker_host()` returns the value to inject as `DOCKER_HOST` for
//! socket/tcp runtimes; web-only runtimes (url field) yield None.

use anyhow::Result;
use rusqlite::Connection;

use utils::path::expand_tilde;

#[derive(Debug, Clone)]
pub struct RuntimeRow {
    pub name: String,
    /// Path to the unix socket (e.g. `~/.colima/default/docker.sock`)
    pub socket_path: Option<String>,
    /// Full DOCKER_HOST URL for TCP remotes (e.g. `tcp://remote:2376`)
    pub host: Option<String>,
    /// HTTP URL for web-based orchestrators (Dockge, Portainer, etc.)
    pub url: Option<String>,
    pub enabled: bool,
}

impl RuntimeRow {
    /// Returns the DOCKER_HOST value to inject into subprocess environments.
    /// Only applies to socket/tcp runtimes — web-based runtimes (url only) return None.
    pub fn docker_host(&self) -> Option<String> {
        if let Some(sock) = &self.socket_path {
            let expanded = expand_tilde(sock);
            Some(format!("unix://{expanded}"))
        } else {
            self.host.clone()
        }
    }
}

pub fn list(conn: &Connection) -> Result<Vec<RuntimeRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, socket_path, host, url, enabled FROM docker_runtimes ORDER BY name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(RuntimeRow {
            name: row.get(0)?,
            socket_path: row.get(1)?,
            host: row.get(2)?,
            url: row.get(3)?,
            enabled: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Returns the first enabled socket/tcp runtime's DOCKER_HOST value for subprocess injection.
/// Web-only runtimes (url, no socket_path/host) are skipped.
pub fn active_host(conn: &Connection) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT socket_path, host FROM docker_runtimes
             WHERE enabled = 1 AND (socket_path IS NOT NULL OR host IS NOT NULL)
             ORDER BY name LIMIT 1",
        )
        .ok()?;
    let (socket_path, host) = stmt
        .query_row([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })
        .ok()?;
    if let Some(sock) = socket_path {
        Some(format!("unix://{}", expand_tilde(&sock)))
    } else {
        host
    }
}

pub fn exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM docker_runtimes WHERE name = ?1",
        rusqlite::params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Insert a brand-new runtime. Errors if `name` already exists — call
/// [`update`] to modify existing rows.
pub fn insert(conn: &Connection, rt: &RuntimeRow) -> Result<()> {
    conn.execute(
        "INSERT INTO docker_runtimes (name, socket_path, host, url, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![rt.name, rt.socket_path, rt.host, rt.url, rt.enabled],
    )?;
    Ok(())
}

/// Update an existing runtime. Errors if `name` is unknown — call
/// [`insert`] to register a new one.
pub fn update(conn: &Connection, rt: &RuntimeRow) -> Result<()> {
    let n = conn.execute(
        "UPDATE docker_runtimes
            SET socket_path = ?2,
                host        = ?3,
                url         = ?4,
                enabled     = ?5
          WHERE name = ?1",
        rusqlite::params![rt.name, rt.socket_path, rt.host, rt.url, rt.enabled],
    )?;
    if n == 0 {
        anyhow::bail!("docker runtime '{}' not found", rt.name);
    }
    Ok(())
}

pub fn remove(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM docker_runtimes WHERE name = ?1",
        rusqlite::params![name],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn runtime_crud() {
        let conn = test_conn();
        let rt = RuntimeRow {
            name: "colima".into(),
            socket_path: Some("~/.colima/default/docker.sock".into()),
            host: None,
            url: None,
            enabled: true,
        };
        insert(&conn, &rt).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "colima");

        let docker_host = rows[0].docker_host().unwrap();
        assert!(docker_host.starts_with("unix://"), "got: {docker_host}");
        assert!(
            !docker_host.contains('~'),
            "tilde should be expanded: {docker_host}"
        );

        assert!(remove(&conn, "colima").unwrap());
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn tcp_host() {
        let conn = test_conn();
        let rt = RuntimeRow {
            name: "remote".into(),
            socket_path: None,
            host: Some("tcp://remote:2376".into()),
            url: None,
            enabled: true,
        };
        insert(&conn, &rt).unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows[0].docker_host().as_deref(), Some("tcp://remote:2376"));
    }

    #[test]
    fn web_url_no_docker_host() {
        let rt = RuntimeRow {
            name: "portainer".into(),
            socket_path: None,
            host: None,
            url: Some("http://portainer:9000".into()),
            enabled: true,
        };
        assert!(
            rt.docker_host().is_none(),
            "web-only runtime should return None for docker_host"
        );
    }

    #[test]
    fn active_host_returns_first_socket() {
        let conn = test_conn();
        insert(
            &conn,
            &RuntimeRow {
                name: "a".into(),
                socket_path: Some("/var/run/docker.sock".into()),
                host: None,
                url: None,
                enabled: true,
            },
        )
        .unwrap();
        let host = active_host(&conn).unwrap();
        assert!(host.starts_with("unix://"));
    }

    #[test]
    fn active_host_none_when_empty() {
        let conn = test_conn();
        assert!(active_host(&conn).is_none());
    }
}
