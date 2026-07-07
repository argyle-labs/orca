//! Plugin install registry — channel/lock policy and reconciliation state per (system, plugin).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

/// Identity placeholder for the local orca node before pod-mesh node identity
/// lands. Mirrors `contract::config::LOCAL_USER` — once each node has a real id, this
/// constant goes away and callers pass the actual `system_id`.
pub const LOCAL_SYSTEM: &str = "local";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginInstallRow {
    pub system_id: String,
    pub plugin_id: String,
    /// "latest" | "latest-rc" | "locked"
    pub channel: String,
    /// Required when `channel == "locked"`; ignored otherwise.
    pub locked_version: Option<String>,
    /// Last resolved version for the channel. The reconciler uses this as
    /// the target for binary sync.
    pub desired_version: Option<String>,
    /// Version actually present on disk; None until the reconciler reports
    /// success.
    pub installed_version: Option<String>,
    pub installed_at: Option<String>,
    pub updated_at: String,
}

/// Set or update the install record for `(system_id, plugin_id)`. Channel +
/// lock policy may be changed at any time; the reconciler picks up changes
/// on its next pass and pulls the matching binary.
pub fn upsert(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
    channel: &str,
    locked_version: Option<&str>,
) -> Result<()> {
    if !matches!(channel, "latest" | "latest-rc" | "locked") {
        anyhow::bail!("channel must be 'latest', 'latest-rc' or 'locked', got '{channel}'");
    }
    if channel == "locked" && locked_version.is_none() {
        anyhow::bail!("locked_version is required when channel = 'locked'");
    }
    conn.execute(
        "INSERT INTO plugin_installs
            (system_id, plugin_id, channel, locked_version, updated_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(system_id, plugin_id) DO UPDATE SET
            channel        = excluded.channel,
            locked_version = excluded.locked_version,
            updated_at     = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![system_id, plugin_id, channel, locked_version],
    )?;
    Ok(())
}

/// Record reconciliation progress: the reconciler resolved `desired_version`
/// and (if successful) installed `installed_version`. `installed_version=None`
/// is allowed — used to mark "resolved but not yet downloaded".
pub fn set_versions(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
    desired_version: Option<&str>,
    installed_version: Option<&str>,
) -> Result<bool> {
    let installed_at = if installed_version.is_some() {
        "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')"
    } else {
        "installed_at"
    };
    let sql = format!(
        "UPDATE plugin_installs SET
            desired_version   = ?3,
            installed_version = ?4,
            installed_at      = {installed_at},
            updated_at        = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE system_id = ?1 AND plugin_id = ?2"
    );
    let n = conn.execute(
        &sql,
        rusqlite::params![system_id, plugin_id, desired_version, installed_version],
    )?;
    Ok(n > 0)
}

/// Remove the install record for `(system_id, plugin_id)`. Returns true if a
/// row was deleted. Does not delete the actual binary — that's the file-sync
/// layer's job to reconcile.
pub fn delete(conn: &Connection, system_id: &str, plugin_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM plugin_installs WHERE system_id = ?1 AND plugin_id = ?2",
        rusqlite::params![system_id, plugin_id],
    )?;
    Ok(n > 0)
}

/// Fetch the install record for `(system_id, plugin_id)`, or None.
pub fn get(
    conn: &Connection,
    system_id: &str,
    plugin_id: &str,
) -> Result<Option<PluginInstallRow>> {
    let mut stmt = conn.prepare(
        "SELECT system_id, plugin_id, channel, locked_version, desired_version,
                installed_version, installed_at, updated_at
         FROM plugin_installs WHERE system_id = ?1 AND plugin_id = ?2",
    )?;
    let row = stmt
        .query_row(rusqlite::params![system_id, plugin_id], |r| {
            Ok(PluginInstallRow {
                system_id: r.get(0)?,
                plugin_id: r.get(1)?,
                channel: r.get(2)?,
                locked_version: r.get(3)?,
                desired_version: r.get(4)?,
                installed_version: r.get(5)?,
                installed_at: r.get(6)?,
                updated_at: r.get(7)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// List every install record for `system_id`. Used by the reconciler to walk
/// the desired plugin set for this node.
pub fn list(conn: &Connection, system_id: &str) -> Result<Vec<PluginInstallRow>> {
    let mut stmt = conn.prepare(
        "SELECT system_id, plugin_id, channel, locked_version, desired_version,
                installed_version, installed_at, updated_at
         FROM plugin_installs WHERE system_id = ?1 ORDER BY plugin_id",
    )?;
    let rows = stmt.query_map([system_id], |r| {
        Ok(PluginInstallRow {
            system_id: r.get(0)?,
            plugin_id: r.get(1)?,
            channel: r.get(2)?,
            locked_version: r.get(3)?,
            desired_version: r.get(4)?,
            installed_version: r.get(5)?,
            installed_at: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn upsert_get_list_delete() {
        let conn = test_conn();

        upsert(&conn, LOCAL_SYSTEM, "dockge", "latest", None).unwrap();
        upsert(
            &conn,
            LOCAL_SYSTEM,
            "homeassistant",
            "locked",
            Some("0.3.1"),
        )
        .unwrap();

        let row = get(&conn, LOCAL_SYSTEM, "dockge").unwrap().unwrap();
        assert_eq!(row.channel, "latest");
        assert!(row.locked_version.is_none());
        assert!(row.installed_version.is_none());

        let locked = get(&conn, LOCAL_SYSTEM, "homeassistant").unwrap().unwrap();
        assert_eq!(locked.channel, "locked");
        assert_eq!(locked.locked_version.as_deref(), Some("0.3.1"));

        // Reconciler reports installed version.
        let updated = set_versions(
            &conn,
            LOCAL_SYSTEM,
            "dockge",
            Some("0.0.1-alpha.1"),
            Some("0.0.1-alpha.1"),
        )
        .unwrap();
        assert!(updated);
        let row = get(&conn, LOCAL_SYSTEM, "dockge").unwrap().unwrap();
        assert_eq!(row.installed_version.as_deref(), Some("0.0.1-alpha.1"));
        assert!(row.installed_at.is_some());

        let all = list(&conn, LOCAL_SYSTEM).unwrap();
        assert_eq!(all.len(), 2);

        // Switching channel back to latest should clear lock requirement.
        upsert(&conn, LOCAL_SYSTEM, "homeassistant", "latest-rc", None).unwrap();
        let row = get(&conn, LOCAL_SYSTEM, "homeassistant").unwrap().unwrap();
        assert_eq!(row.channel, "latest-rc");
        assert!(row.locked_version.is_none());

        assert!(delete(&conn, LOCAL_SYSTEM, "dockge").unwrap());
        assert!(!delete(&conn, LOCAL_SYSTEM, "dockge").unwrap());
    }

    #[test]
    fn rejects_invalid_channel() {
        let conn = test_conn();
        let err = upsert(&conn, LOCAL_SYSTEM, "dockge", "stable", None).unwrap_err();
        assert!(err.to_string().contains("channel"));
    }

    #[test]
    fn locked_requires_version() {
        let conn = test_conn();
        let err = upsert(&conn, LOCAL_SYSTEM, "dockge", "locked", None).unwrap_err();
        assert!(err.to_string().contains("locked_version"));
    }
}
