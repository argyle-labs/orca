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
/// fragment SQL that uniquely identifies the tables carrying it: `routes`
/// (built-in on every endpoint — JSON array, NOT NULL default; a pre-WS2 table
/// carries the legacy `addresses` name and is renamed) and `failover_sources`
/// (nullable ordered secondaries on `managed_mounts`).
/// When adding a new nullable field to an existing `endpoint_resource!` model,
/// add a matching reconcile line here or existing fleet DBs will 500 on the
/// next SELECT.
pub fn apply_fragments(conn: &Connection) -> Result<()> {
    for f in inventory::iter::<SchemaFragment> {
        conn.execute_batch(f.sql)
            .map_err(|e| anyhow::anyhow!("schema fragment `{}` failed to apply: {e}", f.name))?;
        if f.sql.contains("routes TEXT") {
            // Own-table endpoints carry the built-in `routes` JSON column. A
            // table created before the WS2 cleanup has the legacy `addresses`
            // column instead — rename it so the on-disk schema matches the
            // `routes` model (mirrors the shared-table migration 20260728000000);
            // otherwise add the column fresh.
            rename_column_if_present(conn, f.name, "addresses", "routes").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` addresses→routes rename: {e}", f.name)
            })?;
            ensure_column(conn, f.name, "routes", "TEXT NOT NULL DEFAULT '[]'").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` routes migration: {e}", f.name)
            })?;
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
    if column_exists(conn, table, column)? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    Ok(())
}

/// Rename `from` → `to` on `table`, but only when `from` still exists and `to`
/// does not — so it is a safe no-op on a table already migrated (or freshly
/// created with the new name). Data + PK are preserved by `RENAME COLUMN`.
fn rename_column_if_present(conn: &Connection, table: &str, from: &str, to: &str) -> Result<()> {
    if column_exists(conn, table, from)? && !column_exists(conn, table, to)? {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} RENAME COLUMN {from} TO {to};"
        ))?;
    }
    Ok(())
}

/// True when `table` has a column named `column`.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let found = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    Ok(found)
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
    fn ensure_column_adds_missing_column_then_is_idempotent() {
        // A table created before a nullable field was added to its model.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE managed_mounts (name TEXT PRIMARY KEY, updated_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO managed_mounts (name,updated_at) VALUES ('data',1)",
            [],
        )
        .unwrap();
        assert!(!cols(&conn, "managed_mounts").contains(&"failover_sources".to_string()));

        // Reconcile adds the column; existing row backfills to the default (NULL).
        ensure_column(&conn, "managed_mounts", "failover_sources", "TEXT").unwrap();
        assert!(cols(&conn, "managed_mounts").contains(&"failover_sources".to_string()));

        // Idempotent: a second run is a no-op, never errors.
        ensure_column(&conn, "managed_mounts", "failover_sources", "TEXT").unwrap();
        assert_eq!(
            cols(&conn, "managed_mounts")
                .iter()
                .filter(|c| *c == "failover_sources")
                .count(),
            1
        );
    }
}
