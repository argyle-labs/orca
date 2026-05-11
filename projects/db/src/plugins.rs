//! Plugin registry — installed plugins, their MCP transport config, UI hooks, and dep graph.

use anyhow::Result;
use rusqlite::Connection;

use crate::{PluginSearchTool, to_json_arr, to_json_obj};

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id: String,
    pub manifest_path: String,
    pub tier: String,
    /// UI mode this plugin belongs to: "orca" (default) or any custom mode string (e.g. "rebuy").
    /// Plugins with the same mode group together in the sidebar.
    pub mode: String,
    pub mcp_command: Option<String>,
    pub mcp_args: Vec<String>,
    pub mcp_env: std::collections::HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    pub mcp_token_env: Option<String>,
    /// HTTP/SSE endpoints for this plugin's MCP server, tried in priority order.
    /// Allows fallback across public domain → LAN → tailscale addresses.
    /// When non-empty, used instead of spawning a stdio subprocess (deploy mode).
    pub mcp_urls: Vec<String>,
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

const PLUGIN_COLS: &str =
    "id, manifest_path, tier, COALESCE(mode,'orca'), mcp_command, mcp_args, mcp_env,
     context_injection, enabled, command_map, mcp_token_env, COALESCE(nav_links,'[]'),
     COALESCE(search_tools,'[]'), specs_dir, mcp_url";

#[allow(clippy::too_many_arguments)]
fn parse_plugin_row(
    id: String,
    manifest_path: String,
    tier: String,
    mode: String,
    mcp_command: Option<String>,
    args_json: String,
    env_json: String,
    context_injection: String,
    enabled: i32,
    map_json: String,
    mcp_token_env: Option<String>,
    nav_links_json: String,
    search_tools_json: String,
    specs_dir: Option<String>,
    mcp_url_raw: Option<String>,
) -> PluginRow {
    // mcp_url column stores either a JSON array ["url1","url2"] or a plain URL string.
    let mcp_urls = match mcp_url_raw.as_deref() {
        None | Some("") => vec![],
        Some(s) => serde_json::from_str::<Vec<String>>(s).unwrap_or_else(|_| vec![s.to_string()]),
    };
    PluginRow {
        id,
        manifest_path,
        tier,
        mode,
        mcp_command,
        mcp_args: serde_json::from_str(&args_json).unwrap_or_default(),
        mcp_env: serde_json::from_str(&env_json).unwrap_or_default(),
        mcp_token_env,
        mcp_urls,
        context_injection,
        enabled: enabled != 0,
        command_map: serde_json::from_str(&map_json).unwrap_or_default(),
        nav_links: serde_json::from_str(&nav_links_json).unwrap_or_default(),
        search_tools: serde_json::from_str(&search_tools_json).unwrap_or_default(),
        specs_dir,
    }
}

pub fn list(conn: &Connection) -> Result<Vec<PluginRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {PLUGIN_COLS} FROM plugins ORDER BY id"))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i32>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
            row.get::<_, Option<String>>(13)?,
            row.get::<_, Option<String>>(14)?,
        ))
    })?;
    let mut result = Vec::new();
    for r in rows {
        let (
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ) = r?;
        result.push(parse_plugin_row(
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ));
    }
    Ok(result)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<PluginRow>> {
    let result = conn.query_row(
        &format!("SELECT {PLUGIN_COLS} FROM plugins WHERE id = ?1"),
        rusqlite::params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i32>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
            ))
        },
    );
    match result {
        Ok((
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        )) => Ok(Some(parse_plugin_row(
            id,
            manifest_path,
            tier,
            mode,
            mcp_command,
            args_json,
            env_json,
            context_injection,
            enabled,
            map_json,
            mcp_token_env,
            nav_links_json,
            search_tools_json,
            specs_dir,
            mcp_url,
        ))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert(conn: &Connection, plugin: &PluginRow) -> Result<()> {
    let args_json = to_json_arr(&plugin.mcp_args);
    let env_json = to_json_obj(&plugin.mcp_env);
    let map_json = to_json_obj(&plugin.command_map);
    let nav_json = to_json_arr(&plugin.nav_links);
    let search_tools_json = to_json_arr(&plugin.search_tools);
    let mcp_url_json: Option<String> = if plugin.mcp_urls.is_empty() {
        None
    } else {
        Some(to_json_arr(&plugin.mcp_urls))
    };
    conn.execute(
        "INSERT INTO plugins (id, manifest_path, tier, mode, mcp_command, mcp_args, mcp_env, context_injection, enabled, command_map, mcp_token_env, nav_links, search_tools, specs_dir, mcp_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
             manifest_path     = excluded.manifest_path,
             tier              = excluded.tier,
             mode              = excluded.mode,
             mcp_command       = excluded.mcp_command,
             mcp_args          = excluded.mcp_args,
             mcp_env           = excluded.mcp_env,
             context_injection = excluded.context_injection,
             enabled           = excluded.enabled,
             command_map       = excluded.command_map,
             mcp_token_env     = excluded.mcp_token_env,
             nav_links         = excluded.nav_links,
             search_tools      = excluded.search_tools,
             specs_dir         = excluded.specs_dir,
             mcp_url           = excluded.mcp_url",
        rusqlite::params![
            plugin.id, plugin.manifest_path, plugin.tier, plugin.mode,
            plugin.mcp_command, args_json, env_json, plugin.context_injection,
            plugin.enabled as i32, map_json, plugin.mcp_token_env, nav_json, search_tools_json,
            plugin.specs_dir, mcp_url_json,
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
        rusqlite::params![enabled as i32, id],
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
            mode: "orca".into(),
            mcp_command: Some("node".into()),
            mcp_args: vec!["server.js".into()],
            mcp_env: Default::default(),
            mcp_token_env: None,
            mcp_urls: vec![],
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

        upsert(&conn, &make_plugin("rebuy")).unwrap();

        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "rebuy");
        assert_eq!(rows[0].tier, "personal");

        let found = get(&conn, "rebuy").unwrap().unwrap();
        assert_eq!(found.mcp_args, vec!["server.js"]);

        assert!(remove(&conn, "rebuy").unwrap());
        assert!(list(&conn).unwrap().is_empty());
        assert!(!remove(&conn, "rebuy").unwrap());
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
