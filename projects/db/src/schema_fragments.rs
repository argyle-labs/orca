//! Inventory-collected schema fragments.
//!
//! Hand-written tables live in [`crate::apply_schema`]'s big
//! `execute_batch`. Toolkit-generated tables (`endpoint_resource!` and
//! friends) register a [`SchemaFragment`] via `inventory::submit!` so
//! they're picked up automatically at db-open time without anyone having
//! to edit `apply_schema`. See
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]].

use anyhow::Result;
use macro_runtime::SchemaFragment;
use rusqlite::Connection;

/// Apply every registered fragment. Idempotent — each fragment uses
/// `IF NOT EXISTS`. Errors surface with the fragment name so a typo in
/// the macro-emitted SQL points back at the offending plugin.
///
/// After (re)creating tables, reconcile additive columns onto endpoint tables
/// that predate them: `CREATE TABLE IF NOT EXISTS` never alters an existing
/// table, so a table created before a column was added to its model would
/// otherwise be missing that column and every generated SELECT (which lists it)
/// would fail. Each reconciled column is keyed off a marker substring in the
/// fragment SQL that uniquely identifies the tables carrying it: `addresses`
/// (built-in on every endpoint — JSON array, NOT NULL default) and
/// `failover_sources` (nullable ordered secondaries on `managed_mounts`).
/// When adding a new nullable field to an existing `endpoint_resource!` model,
/// add a matching reconcile line here or existing fleet DBs will 500 on the
/// next SELECT.
pub fn apply_fragments(conn: &Connection) -> Result<()> {
    for f in inventory::iter::<SchemaFragment> {
        conn.execute_batch(f.sql)
            .map_err(|e| anyhow::anyhow!("schema fragment `{}` failed to apply: {e}", f.name))?;
        if f.sql.contains("addresses TEXT") {
            ensure_column(conn, f.name, "addresses", "TEXT NOT NULL DEFAULT '[]'").map_err(
                |e| anyhow::anyhow!("schema fragment `{}` addresses migration: {e}", f.name),
            )?;
        }
        if f.sql.contains("failover_sources TEXT") {
            ensure_column(conn, f.name, "failover_sources", "TEXT").map_err(|e| {
                anyhow::anyhow!(
                    "schema fragment `{}` failover_sources migration: {e}",
                    f.name
                )
            })?;
        }
    }
    Ok(())
}

/// Add `column` to `table` if absent. No-op when the column already exists,
/// so it is safe to run on every db open.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}
