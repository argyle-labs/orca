//! Plugin-declared TypedValue type registry.

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginTypeRow {
    pub plugin_id: String,
    pub plugin_namespace: String,
    pub type_name: String,
    pub fq_type_id: String,
    pub schema_version: String,
    /// Raw JSON Schema text, exactly as the plugin submitted it.
    pub schema_json: String,
    /// "general" | "sensitive"
    pub sensitivity: String,
    pub declared_at: String,
}

#[derive(Debug)]
struct NamespaceCollision {
    fq_type_id: String,
    owner_plugin_id: String,
}

impl std::fmt::Display for NamespaceCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "type '{}' already declared by plugin '{}'",
            self.fq_type_id, self.owner_plugin_id
        )
    }
}

impl std::error::Error for NamespaceCollision {}

/// If `err` is a namespace-collision error from [`upsert`], returns the
/// `(fq_type_id, owner_plugin_id)` so callers can surface a clean message.
pub fn is_namespace_collision(err: &anyhow::Error) -> Option<(String, String)> {
    err.downcast_ref::<NamespaceCollision>()
        .map(|c| (c.fq_type_id.clone(), c.owner_plugin_id.clone()))
}

/// Upsert a plugin-declared TypedValue type. The fully-qualified id is
/// computed as `<plugin_namespace>.<type_name>` and is unique across all
/// plugins. A different `plugin_id` already owning the same fq id is
/// rejected loudly — see [`is_namespace_collision`].
pub fn upsert(
    conn: &Connection,
    plugin_id: &str,
    plugin_namespace: &str,
    type_name: &str,
    schema_version: &str,
    schema_json: &str,
    sensitivity: &str,
) -> Result<()> {
    if !matches!(sensitivity, "general" | "sensitive") {
        anyhow::bail!("sensitivity must be 'general' or 'sensitive', got '{sensitivity}'");
    }
    let fq = format!("{plugin_namespace}.{type_name}");

    let mut owner_stmt = conn.prepare(
        "SELECT plugin_id FROM plugin_types WHERE fq_type_id = ?1 AND plugin_id != ?2 LIMIT 1",
    )?;
    let owner = owner_stmt
        .query_row(rusqlite::params![fq, plugin_id], |r| r.get::<_, String>(0))
        .optional()?;
    if let Some(owner_id) = owner {
        return Err(anyhow!(NamespaceCollision {
            fq_type_id: fq,
            owner_plugin_id: owner_id,
        }));
    }

    conn.execute(
        "INSERT INTO plugin_types
            (plugin_id, plugin_namespace, type_name, fq_type_id, schema_version, schema_json, sensitivity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(plugin_id, type_name) DO UPDATE SET
            plugin_namespace = excluded.plugin_namespace,
            fq_type_id     = excluded.fq_type_id,
            schema_version = excluded.schema_version,
            schema_json    = excluded.schema_json,
            sensitivity    = excluded.sensitivity,
            declared_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            plugin_id,
            plugin_namespace,
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
        "SELECT plugin_id, plugin_namespace, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE plugin_id = ?1 ORDER BY type_name",
    )?;
    let rows = stmt.query_map([plugin_id], row_from)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Look up a single type by its fully-qualified id (`<plugin_namespace>.<type_name>`).
pub fn get(conn: &Connection, fq_type_id: &str) -> Result<Option<PluginTypeRow>> {
    let mut stmt = conn.prepare(
        "SELECT plugin_id, plugin_namespace, type_name, fq_type_id, schema_version, schema_json, sensitivity, declared_at
         FROM plugin_types WHERE fq_type_id = ?1",
    )?;
    let row = stmt.query_row([fq_type_id], row_from).optional()?;
    Ok(row)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<PluginTypeRow> {
    Ok(PluginTypeRow {
        plugin_id: r.get(0)?,
        plugin_namespace: r.get(1)?,
        type_name: r.get(2)?,
        fq_type_id: r.get(3)?,
        schema_version: r.get(4)?,
        schema_json: r.get(5)?,
        sensitivity: r.get(6)?,
        declared_at: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn fq_type_id_uses_namespace() {
        let conn = test_conn();
        upsert(&conn, "sonarr-alpha", "arr", "Show", "1", "{}", "general").unwrap();
        let row = get(&conn, "arr.Show").unwrap().expect("found");
        assert_eq!(row.plugin_namespace, "arr");
        assert_eq!(row.fq_type_id, "arr.Show");
    }

    #[test]
    fn collision_across_plugins_is_rejected() {
        let conn = test_conn();
        upsert(&conn, "sonarr-alpha", "arr", "Show", "1", "{}", "general").unwrap();
        let err = upsert(&conn, "sonarr-echo", "arr", "Show", "1", "{}", "general")
            .expect_err("collision must reject");
        let (fq, owner) = is_namespace_collision(&err).expect("typed collision");
        assert_eq!(fq, "arr.Show");
        assert_eq!(owner, "sonarr-alpha");
    }
}
