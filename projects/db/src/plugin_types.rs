//! Plugin-declared TypedValue type registry.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginTypeRow {
    pub plugin_id: String,
    pub type_name: String,
    pub fq_type_id: String,
    pub schema_version: String,
    /// Raw JSON Schema text, exactly as the plugin submitted it.
    pub schema_json: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

/// Upsert a plugin-declared TypedValue type. The fully-qualified id is
/// computed as `<plugin_id>.<type_name>` and is unique across all plugins.
pub fn upsert(
    conn: &Connection,
    plugin_id: &str,
    type_name: &str,
    schema_version: &str,
    schema_json: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_id}.{type_name}");
    conn.execute(
        "INSERT INTO plugin_types
            (plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(plugin_id, type_name) DO UPDATE SET
            schema_version = excluded.schema_version,
            schema_json    = excluded.schema_json,
            sensitivity    = excluded.sensitivity,
            declared_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            plugin_id,
            type_name,
            fq,
            schema_version,
            schema_json,
            sensitivity
        ],
    )?;
    Ok(())
}

/// List all types declared by a single plugin.
pub fn list(conn: &Connection, plugin_id: &str) -> Result<Vec<PluginTypeRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE plugin_id = ?1 ORDER BY type_name",
    )?;
    let rows = stmt.query_map([plugin_id], |r| {
        Ok(PluginTypeRow {
            plugin_id: r.get(0)?,
            type_name: r.get(1)?,
            fq_type_id: r.get(2)?,
            schema_version: r.get(3)?,
            schema_json: r.get(4)?,
            sensitivity: r.get(5)?,
            declared_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single type by its fully-qualified id (`<plugin_id>.<type_name>`).
pub fn get(conn: &Connection, fq_type_id: &str) -> Result<Option<PluginTypeRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE fq_type_id = ?1",
    )?;
    let row = stmt
        .query_row([fq_type_id], |r| {
            Ok(PluginTypeRow {
                plugin_id: r.get(0)?,
                type_name: r.get(1)?,
                fq_type_id: r.get(2)?,
                schema_version: r.get(3)?,
                schema_json: r.get(4)?,
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
