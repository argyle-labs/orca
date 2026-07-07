//! Plugin-declared tool registry — what the MCP layer surfaces to LLMs as plugin-owned tools.

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginToolRow {
    pub plugin_id: String,
    pub plugin_namespace: String,
    pub name: String,
    pub fq_name: String,
    pub description: String,
    /// Raw JSON Schema text describing the tool's input arguments.
    pub input_schema: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

/// Returned by `replace` when a tool's fq_name (`<namespace>.<name>`) is
/// already declared by a *different* `plugin_id`. The reject-on-collision
/// policy means two plugins sharing a namespace cannot declare the same
/// tool — ops must rename or pick distinct namespaces.
pub fn is_namespace_collision(err: &anyhow::Error) -> Option<(String, String)> {
    err.downcast_ref::<NamespaceCollision>()
        .map(|c| (c.fq_name.clone(), c.owner_plugin_id.clone()))
}

#[derive(Debug)]
struct NamespaceCollision {
    fq_name: String,
    owner_plugin_id: String,
}

impl std::fmt::Display for NamespaceCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tool '{}' already declared by plugin '{}'",
            self.fq_name, self.owner_plugin_id
        )
    }
}

impl std::error::Error for NamespaceCollision {}

/// Upsert a plugin-declared tool. Fully-qualified name is `<plugin_namespace>.<name>`
/// and is unique across all plugins. Re-declaring the same `(plugin_id, name)`
/// updates the description / schema / sensitivity in place.
pub fn upsert(
    conn: &Connection,
    plugin_id: &str,
    plugin_namespace: &str,
    name: &str,
    description: &str,
    input_schema: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_namespace}.{name}");
    if let Some(owner) = collision_owner(conn, &fq, plugin_id)? {
        return Err(anyhow!(NamespaceCollision {
            fq_name: fq,
            owner_plugin_id: owner,
        }));
    }
    conn.execute(
        "INSERT INTO plugin_tools
            (plugin_id, plugin_namespace, name, fq_name, description, input_schema, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(plugin_id, name) DO UPDATE SET
            plugin_namespace = excluded.plugin_namespace,
            fq_name          = excluded.fq_name,
            description      = excluded.description,
            input_schema     = excluded.input_schema,
            sensitivity      = excluded.sensitivity,
            declared_at      = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            plugin_id,
            plugin_namespace,
            name,
            fq,
            description,
            input_schema,
            sensitivity
        ],
    )?;
    Ok(())
}

/// Replace the entire tool set for a plugin. The host invokes this when
/// `orca/tools.declare` arrives — declarations are idempotent and replace
/// the previously-known set, so any tool the plugin no longer declares is
/// removed from the registry.
///
/// If any incoming tool's fq_name (`<namespace>.<name>`) is already owned by
/// a *different* plugin_id, the whole batch is rejected with a
/// `NamespaceCollision` error (see [`is_namespace_collision`]). The reject
/// happens inside the transaction so the plugin's existing rows are
/// preserved on failure.
pub fn replace(
    conn: &mut Connection,
    plugin_id: &str,
    plugin_namespace: &str,
    tools: &[(String, String, String, String)],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM plugin_tools WHERE plugin_id = ?1", [plugin_id])?;
    for (name, description, schema, sensitivity) in tools {
        if !matches!(sensitivity.as_str(), "general" | "sensitive") {
            anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
        }
        let fq = format!("{plugin_namespace}.{name}");
        if let Some(owner) = collision_owner(&tx, &fq, plugin_id)? {
            return Err(anyhow!(NamespaceCollision {
                fq_name: fq,
                owner_plugin_id: owner,
            }));
        }
        tx.execute(
            "INSERT INTO plugin_tools
                (plugin_id, plugin_namespace, name, fq_name, description, input_schema, sensitivity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                plugin_id,
                plugin_namespace,
                name,
                fq,
                description,
                schema,
                sensitivity
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Returns the `plugin_id` that currently owns `fq_name` if it's owned by
/// someone other than `self_plugin_id`. We just deleted self's rows in
/// `replace`, so any survivor is by definition a different plugin.
fn collision_owner(
    conn: &Connection,
    fq_name: &str,
    self_plugin_id: &str,
) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id FROM plugin_tools WHERE fq_name = ?1 AND plugin_id != ?2 LIMIT 1",
    )?;
    let owner = stmt
        .query_row(rusqlite::params![fq_name, self_plugin_id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    Ok(owner)
}

/// List all tools declared by a single plugin.
pub fn list(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, plugin_namespace, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE plugin_id = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map([plugin_id], row_from)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// List every tool across every plugin — what the MCP registry needs to
/// surface plugin tools to LLMs.
pub fn list_all(conn: &Connection) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, plugin_namespace, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools ORDER BY fq_name",
    )?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single tool by its fully-qualified name (`<plugin_namespace>.<name>`).
pub fn get(conn: &Connection, fq_name: &str) -> Result<Option<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, plugin_namespace, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE fq_name = ?1",
    )?;
    let row = stmt.query_row([fq_name], row_from).optional()?;
    Ok(row)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<PluginToolRow> {
    Ok(PluginToolRow {
        plugin_id: r.get(0)?,
        plugin_namespace: r.get(1)?,
        name: r.get(2)?,
        fq_name: r.get(3)?,
        description: r.get(4)?,
        input_schema: r.get(5)?,
        sensitivity: r.get(6)?,
        declared_at: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn tool(name: &str) -> (String, String, String, String) {
        (
            name.to_string(),
            format!("desc {name}"),
            "{}".to_string(),
            "general".to_string(),
        )
    }

    #[test]
    fn fq_name_uses_namespace_not_plugin_id() {
        let mut conn = test_conn();
        replace(&mut conn, "sonarr-alpha", "arr", &[tool("list-shows")]).unwrap();
        let row = get(&conn, "arr.list-shows").unwrap().expect("found");
        assert_eq!(row.plugin_id, "sonarr-alpha");
        assert_eq!(row.plugin_namespace, "arr");
        assert_eq!(row.fq_name, "arr.list-shows");
        assert!(get(&conn, "sonarr-alpha.list-shows").unwrap().is_none());
    }

    #[test]
    fn two_plugins_share_namespace_with_distinct_tool_names() {
        let mut conn = test_conn();
        replace(&mut conn, "sonarr-alpha", "arr", &[tool("shows")]).unwrap();
        replace(&mut conn, "radarr-echo", "arr", &[tool("movies")]).unwrap();
        assert!(get(&conn, "arr.shows").unwrap().is_some());
        assert!(get(&conn, "arr.movies").unwrap().is_some());
    }

    #[test]
    fn collision_across_plugins_is_rejected() {
        let mut conn = test_conn();
        replace(&mut conn, "sonarr-alpha", "arr", &[tool("list")]).unwrap();
        let err = replace(&mut conn, "sonarr-echo", "arr", &[tool("list")])
            .expect_err("collision must reject");
        let (fq, owner) = is_namespace_collision(&err).expect("typed collision");
        assert_eq!(fq, "arr.list");
        assert_eq!(owner, "sonarr-alpha");
        // Original owner's row is preserved.
        let row = get(&conn, "arr.list").unwrap().unwrap();
        assert_eq!(row.plugin_id, "sonarr-alpha");
        // Loser's other rows weren't half-written.
        assert!(list(&conn, "sonarr-echo").unwrap().is_empty());
    }

    #[test]
    fn same_plugin_redeclare_is_idempotent() {
        let mut conn = test_conn();
        replace(
            &mut conn,
            "sonarr-alpha",
            "arr",
            &[tool("list"), tool("get")],
        )
        .unwrap();
        replace(&mut conn, "sonarr-alpha", "arr", &[tool("list")]).unwrap();
        let rows = list(&conn, "sonarr-alpha").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fq_name, "arr.list");
    }

    #[test]
    fn plugin_can_change_its_own_namespace() {
        let mut conn = test_conn();
        replace(&mut conn, "p", "old", &[tool("t")]).unwrap();
        replace(&mut conn, "p", "new", &[tool("t")]).unwrap();
        assert!(get(&conn, "old.t").unwrap().is_none());
        assert!(get(&conn, "new.t").unwrap().is_some());
    }
}
