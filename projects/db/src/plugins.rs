//! Plugin registry — installed plugins, UI hooks, and dep graph.
//!
//! Transport (command/args/env/urls/token_env) is plugin-authored and lives in
//! the manifest at `manifest_path`. Use [`crate::plugin_manifest::parse_path`]
//! at dial time to resolve it — those facts are not cached here.

use anyhow::Result;
use rusqlite::Connection;

use crate::{PluginSearchTool, to_json_arr, to_json_obj};

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub manifest_path: String,
    pub tier: String,
    pub context_injection: String,
    pub enabled: bool,
    /// Maps universal command name → plugin's internal MCP tool name.
    pub command_map: std::collections::HashMap<String, String>,
    /// Sidebar nav links this plugin contributes when its mode is active.
    /// JSON array of {href, label} objects, optionally with {section} for grouping.
    // Plugin-defined nav link objects are free-form; no fixed schema across all plugins.
    #[allow(clippy::disallowed_types)]
    pub nav_links: Vec<serde_json::Value>,
    /// MCP tools this plugin exposes for orca's unified search (Cmd+K).
    pub search_tools: Vec<PluginSearchTool>,
    /// Filesystem path to the directory containing this plugin's spec files.
    /// Files here are served by orca's spec system with the plugin's id as namespace.
    pub specs_dir: Option<String>,
}

const PLUGIN_COLS: &str = "id, manifest_path, tier, context_injection, enabled, command_map,
     COALESCE(nav_links,'[]'), COALESCE(search_tools,'[]'), specs_dir";

fn row_to_plugin(row: &rusqlite::Row<'_>) -> rusqlite::Result<PluginRow> {
    let command_map_json: String = row.get(5)?;
    let nav_links_json: String = row.get(6)?;
    let search_tools_json: String = row.get(7)?;
    Ok(PluginRow {
        id: row.get(0)?,
        manifest_path: row.get(1)?,
        tier: row.get(2)?,
        context_injection: row.get(3)?,
        enabled: row.get(4)?,
        command_map: serde_json::from_str(&command_map_json).unwrap_or_default(),
        nav_links: serde_json::from_str(&nav_links_json).unwrap_or_default(),
        search_tools: serde_json::from_str(&search_tools_json).unwrap_or_default(),
        specs_dir: row.get(8)?,
    })
}

pub fn list(conn: &Connection) -> Result<Vec<PluginRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {PLUGIN_COLS} FROM plugins ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_plugin)?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<PluginRow>> {
    match conn.query_row(
        &format!("SELECT {PLUGIN_COLS} FROM plugins WHERE id = ?1"),
        rusqlite::params![id],
        row_to_plugin,
    ) {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert(conn: &Connection, plugin: &PluginRow) -> Result<()> {
    let map_json = to_json_obj(&plugin.command_map);
    let nav_json = to_json_arr(&plugin.nav_links);
    let search_tools_json = to_json_arr(&plugin.search_tools);
    conn.execute(
        "INSERT INTO plugins (id, manifest_path, tier, context_injection, enabled, command_map, nav_links, search_tools, specs_dir)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET
             manifest_path     = excluded.manifest_path,
             tier              = excluded.tier,
             context_injection = excluded.context_injection,
             enabled           = excluded.enabled,
             command_map       = excluded.command_map,
             nav_links         = excluded.nav_links,
             search_tools      = excluded.search_tools,
             specs_dir         = excluded.specs_dir",
        rusqlite::params![
            plugin.id, plugin.manifest_path, plugin.tier, plugin.context_injection,
            plugin.enabled, map_json, nav_json, search_tools_json, plugin.specs_dir,
        ],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM plugins WHERE id = ?1", rusqlite::params![id])?;
    Ok(n > 0)
}

/// Record that `dep_id` was installed as a dependency of `parent_id`.
pub fn add_dep(conn: &Connection, parent_id: &str, dep_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO plugin_deps (parent_id, dep_id) VALUES (?1, ?2)",
        rusqlite::params![parent_id, dep_id],
    )?;
    Ok(())
}

/// Return all dep_ids that were pulled in by `parent_id`.
pub fn list_deps(conn: &Connection, parent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT dep_id FROM plugin_deps WHERE parent_id = ?1")?;
    let rows = stmt.query_map(rusqlite::params![parent_id], |r| r.get(0))?;
    rows.map(|r| r.map_err(Into::into)).collect()
}

/// Remove all dep records for `parent_id` (called when parent is removed).
pub fn remove_deps(conn: &Connection, parent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM plugin_deps WHERE parent_id = ?1",
        rusqlite::params![parent_id],
    )?;
    Ok(())
}

/// Return true if `dep_id` is depended on by any other plugin.
pub fn has_parent(conn: &Connection, dep_id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM plugin_deps WHERE dep_id = ?1",
        rusqlite::params![dep_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<bool> {
    let n = conn.execute(
        "UPDATE plugins SET enabled = ?1 WHERE id = ?2",
        rusqlite::params![enabled, id],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn make_plugin(id: &str) -> PluginRow {
        PluginRow {
            id: id.into(),
            manifest_path: format!("/plugins/{id}/manifest.toml"),
            tier: "personal".into(),
            context_injection: "minimal".into(),
            enabled: true,
            command_map: Default::default(),
            nav_links: vec![],
            search_tools: vec![],
            specs_dir: None,
        }
    }

    #[test]
    fn crud() {
        let conn = test_conn();
        assert!(list(&conn).unwrap().is_empty());

        upsert(&conn, &make_plugin("acme")).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "acme");
        assert_eq!(rows[0].tier, "personal");

        let found = get(&conn, "acme").unwrap().unwrap();
        assert_eq!(found.manifest_path, "/plugins/acme/manifest.toml");

        assert!(remove(&conn, "acme").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(!remove(&conn, "acme").unwrap());
    }

    #[test]
    fn get_returns_none_for_missing() {
        let conn = test_conn();
        assert!(get(&conn, "ghost").unwrap().is_none());
    }

    #[test]
    fn enabled_toggle() {
        let conn = test_conn();
        upsert(&conn, &make_plugin("p1")).unwrap();

        assert!(set_enabled(&conn, "p1", false).unwrap());
        let p = get(&conn, "p1").unwrap().unwrap();
        assert!(!p.enabled);

        assert!(set_enabled(&conn, "p1", true).unwrap());
        let p = get(&conn, "p1").unwrap().unwrap();
        assert!(p.enabled);

        assert!(!set_enabled(&conn, "nonexistent", true).unwrap());
    }

    #[test]
    fn deps_tracking() {
        let conn = test_conn();
        upsert(&conn, &make_plugin("parent")).unwrap();
        upsert(&conn, &make_plugin("dep-a")).unwrap();
        upsert(&conn, &make_plugin("dep-b")).unwrap();

        add_dep(&conn, "parent", "dep-a").unwrap();
        add_dep(&conn, "parent", "dep-b").unwrap();

        let deps = list_deps(&conn, "parent").unwrap();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&"dep-a".to_string()));

        assert!(has_parent(&conn, "dep-a").unwrap());
        assert!(!has_parent(&conn, "parent").unwrap());

        remove_deps(&conn, "parent").unwrap();
        assert!(list_deps(&conn, "parent").unwrap().is_empty());
    }
}
