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
/// After (re)creating tables, reconcile the built-in `addresses` column onto
/// any endpoint table that predates it: `CREATE TABLE IF NOT EXISTS` never
/// alters an existing table, so a table created before `addresses` became a
/// built-in `endpoint_resource!` column would otherwise be missing it and
/// every generated SELECT (which lists `addresses`) would fail. The macro
/// always emits `addresses TEXT` for endpoint tables, so that marker in the
/// fragment SQL identifies exactly the tables that need the column.
pub fn apply_fragments(conn: &Connection) -> Result<()> {
    for f in inventory::iter::<SchemaFragment> {
        conn.execute_batch(f.sql)
            .map_err(|e| anyhow::anyhow!("schema fragment `{}` failed to apply: {e}", f.name))?;
        if f.sql.contains("addresses TEXT") {
            ensure_column(conn, f.name, "addresses", "TEXT NOT NULL DEFAULT '[]'").map_err(
                |e| anyhow::anyhow!("schema fragment `{}` addresses migration: {e}", f.name),
            )?;
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
