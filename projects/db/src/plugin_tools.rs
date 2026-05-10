//! Plugin-declared tool registry — what the MCP layer surfaces to LLMs as plugin-owned tools.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginToolRow {
    pub plugin_id: String,
    pub name: String,
    pub fq_name: String,
    pub description: String,
    /// Raw JSON Schema text describing the tool's input arguments.
    pub input_schema: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

/// Upsert a plugin-declared tool. Fully-qualified name is `<plugin_id>.<name>`
/// and is unique across all plugins. Re-declaring the same `(plugin_id, name)`
/// updates the description / schema / sensitivity in place.
pub fn upsert(
    conn: &Connection,
    plugin_id: &str,
    name: &str,
    description: &str,
    input_schema: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_id}.{name}");
    conn.execute(
        "INSERT INTO plugin_tools
            (plugin_id, name, fq_name, description, input_schema, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(plugin_id, name) DO UPDATE SET
            description    = excluded.description,
            input_schema   = excluded.input_schema,
            sensitivity    = excluded.sensitivity,
            declared_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![plugin_id, name, fq, description, input_schema, sensitivity],
    )?;
    Ok(())
}

/// Replace the entire tool set for a plugin. The host invokes this when
/// `orca/tools.declare` arrives — declarations are idempotent and replace
/// the previously-known set, so any tool the plugin no longer declares is
/// removed from the registry.
pub fn replace(
    conn: &mut Connection,
    plugin_id: &str,
    tools: &[(String, String, String, String)],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM plugin_tools WHERE plugin_id = ?1", [plugin_id])?;
    for (name, description, schema, sensitivity) in tools {
        if !matches!(sensitivity.as_str(), "general" | "sensitive") {
            anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
        }
        let fq = format!("{plugin_id}.{name}");
        tx.execute(
            "INSERT INTO plugin_tools
                (plugin_id, name, fq_name, description, input_schema, sensitivity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![plugin_id, name, fq, description, schema, sensitivity],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// List all tools declared by a single plugin.
pub fn list(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE plugin_id = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map([plugin_id], |r| {
        Ok(PluginToolRow {
            plugin_id: r.get(0)?,
            name: r.get(1)?,
            fq_name: r.get(2)?,
            description: r.get(3)?,
            input_schema: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// List every tool across every plugin — what the MCP registry needs to
/// surface plugin tools to LLMs.
pub fn list_all(conn: &Connection) -> Result<Vec<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools ORDER BY fq_name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PluginToolRow {
            plugin_id: r.get(0)?,
            name: r.get(1)?,
            fq_name: r.get(2)?,
            description: r.get(3)?,
            input_schema: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single tool by its fully-qualified name (`<plugin_id>.<name>`).
pub fn get(conn: &Connection, fq_name: &str) -> Result<Option<PluginToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, name, fq_name, description, input_schema, sensitivity, declared_at
         FROM plugin_tools WHERE fq_name = ?1",
    )?;
    let row = stmt
        .query_row([fq_name], |r| {
            Ok(PluginToolRow {
                plugin_id: r.get(0)?,
                name: r.get(1)?,
                fq_name: r.get(2)?,
                description: r.get(3)?,
                input_schema: r.get(4)?,
                sensitivity: r.get(5)?,
                declared_at: r.get(6)?,
            })
        })
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    Ok(row)
}
