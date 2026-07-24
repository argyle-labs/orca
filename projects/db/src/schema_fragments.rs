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
        // Replication tombstone flag (`endpoint_resource!(… lww = …)` tables). A
        // table created before tombstones existed (e.g. shares/mounts on rc.26)
        // must gain the column or every write's `deleted` bind and every read's
        // tombstone filter would fail. The marker is the exact column decl the
        // macro emits only for replicated tables.
        if f.sql.contains("deleted INTEGER NOT NULL DEFAULT 0") {
            ensure_column(conn, f.name, "deleted", "INTEGER NOT NULL DEFAULT 0").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` deleted migration: {e}", f.name)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect()
    }

    #[test]
    fn ensure_column_adds_missing_deleted_then_is_idempotent() {
        // A table created before tombstones existed (no `deleted`), holding a row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE shares (name TEXT PRIMARY KEY, updated_at INTEGER);")
            .unwrap();
        conn.execute("INSERT INTO shares (name,updated_at) VALUES ('data',1)", [])
            .unwrap();
        assert!(!cols(&conn, "shares").contains(&"deleted".to_string()));

        // Reconcile adds the NOT NULL column with its default (existing row = 0).
        ensure_column(&conn, "shares", "deleted", "INTEGER NOT NULL DEFAULT 0").unwrap();
        assert!(cols(&conn, "shares").contains(&"deleted".to_string()));
        let d: i64 = conn
            .query_row("SELECT deleted FROM shares WHERE name='data'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(d, 0, "existing row backfilled to live (not tombstoned)");

        // Idempotent: a second run is a no-op, never errors.
        ensure_column(&conn, "shares", "deleted", "INTEGER NOT NULL DEFAULT 0").unwrap();
        assert_eq!(
            cols(&conn, "shares")
                .iter()
                .filter(|c| *c == "deleted")
                .count(),
            1
        );
    }
}
