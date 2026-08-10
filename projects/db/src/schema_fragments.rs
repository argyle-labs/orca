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
use rusqlite::{Connection, OptionalExtension};

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
/// carries the legacy `addresses` name, or the legacy `sources` name on
/// `shares`, and is renamed then any leftover `sources` is dropped) and
/// `failover_sources` (nullable ordered secondaries on `managed_mounts`). The
/// hand-written `mounts` table additionally reconciles the `health`,
/// `active_route`, and `remount_policy` columns added in #252.
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
            // The `shares` table shipped (#132) with a `sources TEXT NOT NULL`
            // column that the endpoint model renamed to the built-in `routes`.
            // Only `addresses` was ever migrated, so a legacy `shares` kept a
            // stranded `sources NOT NULL` column: the replication merge's INSERT
            // omits `sources`, faulting the NOT-NULL constraint, so a
            // controller-authored share (with its routes) never lands on the
            // peer. Fold the legacy column onto `routes` (its host:/export data
            // reads as an empty typed set, which the next authored write
            // replaces) so the merge insert succeeds.
            rename_column_if_present(conn, f.name, "sources", "routes").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` sources→routes rename: {e}", f.name)
            })?;
            ensure_column(conn, f.name, "routes", "TEXT NOT NULL DEFAULT '[]'").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` routes migration: {e}", f.name)
            })?;
            // A table that had already gained a fresh `routes` (via a prior
            // ensure_column) still carries the stranded legacy `sources NOT NULL`
            // column — the rename above is a no-op once `routes` exists. Drop it
            // so the replication merge insert (which omits `sources`) no longer
            // faults its NOT-NULL constraint.
            drop_column_if_present(conn, f.name, "sources").map_err(|e| {
                anyhow::anyhow!("schema fragment `{}` drop legacy sources: {e}", f.name)
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
        if f.name == "mounts" {
            migrate_mounts_to_id_pk(conn, f.sql)
                .map_err(|e| anyhow::anyhow!("schema fragment `mounts` id-PK migration: {e}"))?;
            // #252 added the health/active_route (and typed remount_policy)
            // columns to the CREATE, but `CREATE TABLE IF NOT EXISTS` is a no-op
            // on a table already carrying `id` (a post-#250 table), so these
            // columns were never added. A `SELECT *` then returns rows missing the
            // NOT-NULL `health` key (read as Null → "expected text, got Null"),
            // AND the replication merge's `INSERT … (…, health, active_route, …)`
            // faults with "no column named health" — so mount placements never
            // land on those peers. Ensure them additively (idempotent).
            for (col, decl) in [
                ("remount_policy", "TEXT"),
                ("health", "TEXT NOT NULL DEFAULT 'missing'"),
                ("active_route", "TEXT"),
            ] {
                ensure_column(conn, "mounts", col, decl).map_err(|e| {
                    anyhow::anyhow!("schema fragment `mounts` {col} migration: {e}")
                })?;
            }
        }
    }
    Ok(())
}

/// Rekey a pre-existing `mounts` table from a `name` primary key to a minted
/// uuidv7 `id` primary key. The original table used `name` (e.g. `baldur-data`)
/// as the PK; the model now keys by `id`, makes `name` a per-host label
/// (`UNIQUE(host, name)`), and de-prefixes the legacy `<host>-<role>` names to
/// the bare role (`baldur-data` → `data`). SQLite cannot alter a PK in place, so
/// the table is rebuilt. No-op once the table already carries an `id` column
/// (fresh DBs and already-migrated ones), so it is safe on every db open.
fn migrate_mounts_to_id_pk(conn: &Connection, create_sql: &str) -> Result<()> {
    // A legacy `mounts` row: (name, share_id, host, target, remount_policy, routes, enabled).
    type LegacyMount = (String, String, String, String, Option<String>, String, i64);

    // The fragment's `CREATE TABLE IF NOT EXISTS` already ran: a fresh DB has the
    // new schema (with `id`); a legacy DB kept its old `name`-PK table untouched.
    if !table_exists(conn, "mounts")? || column_exists(conn, "mounts", "id")? {
        return Ok(());
    }

    // Read every legacy row, then rebuild into the new-schema table.
    let legacy: Vec<LegacyMount> = {
        let mut stmt = conn.prepare(
            "SELECT name, share_id, host, target, remount_policy, routes, enabled FROM mounts",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        rows.filter_map(std::result::Result::ok).collect()
    };

    conn.execute_batch("ALTER TABLE mounts RENAME TO mounts_legacy;")?;
    conn.execute_batch(create_sql)?;
    for (name, share_id, host, target, remount_policy, routes, enabled) in legacy {
        conn.execute(
            "INSERT INTO mounts \
             (id, name, share_id, host, target, remount_policy, routes, enabled) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                utils::id::new(),
                deprefix_mount_name(&name),
                share_id,
                host,
                target,
                remount_policy,
                routes,
                enabled,
            ],
        )?;
    }
    conn.execute_batch("DROP TABLE mounts_legacy;")?;
    Ok(())
}

/// Strip a legacy `<host>-` prefix off a mount name when the remainder is a
/// known role (`data` / `backups` / `downloads`), so `baldur-data` → `data` and
/// `freyr-downloads` → `downloads`. A bare role or an unrecognised name is
/// returned unchanged. After de-prefixing, `UNIQUE(host, name)` holds because
/// each host declares at most one placement per role.
fn deprefix_mount_name(name: &str) -> String {
    for role in ["data", "backups", "downloads"] {
        if let Some(prefix) = name.strip_suffix(role)
            && let Some(host) = prefix.strip_suffix('-')
            && !host.is_empty()
        {
            return role.to_string();
        }
    }
    name.to_string()
}

/// True when a table named `table` exists.
fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let found: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    Ok(found)
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

/// Drop `column` from `table` when present (SQLite ≥ 3.35 `DROP COLUMN`). A
/// safe no-op once the column is gone, so it runs on every db open. Used to
/// retire a stranded legacy column (`shares.sources`) that a rename already
/// superseded but left behind on tables that had gained the new column first.
fn drop_column_if_present(conn: &Connection, table: &str, column: &str) -> Result<()> {
    if column_exists(conn, table, column)? {
        conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column};"))?;
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
    fn deprefix_strips_host_prefix_for_known_roles_only() {
        assert_eq!(deprefix_mount_name("baldur-data"), "data");
        assert_eq!(deprefix_mount_name("freyr-downloads"), "downloads");
        assert_eq!(deprefix_mount_name("baldur-backups"), "backups");
        // Already-bare roles are untouched.
        assert_eq!(deprefix_mount_name("data"), "data");
        assert_eq!(deprefix_mount_name("backups"), "backups");
        // Unrecognised remainder → returned verbatim (no accidental collapse).
        assert_eq!(deprefix_mount_name("baldur-media"), "baldur-media");
        assert_eq!(deprefix_mount_name("-data"), "-data");
    }

    #[test]
    fn mounts_id_pk_migration_rebuilds_and_deprefixes() {
        let conn = Connection::open_in_memory().unwrap();
        // Legacy `name`-PK table with a host-prefixed name.
        conn.execute_batch(
            "CREATE TABLE mounts (\
                name TEXT PRIMARY KEY, share_id TEXT NOT NULL, host TEXT NOT NULL, \
                target TEXT NOT NULL, remount_policy TEXT, routes TEXT NOT NULL DEFAULT '[]', \
                enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT, updated_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mounts (name, share_id, host, target, enabled) \
             VALUES ('baldur-data', 's1', 'baldur', '/mnt/data', 1)",
            [],
        )
        .unwrap();

        let new_sql = "CREATE TABLE IF NOT EXISTS mounts (\
            id TEXT PRIMARY KEY, name TEXT NOT NULL, share_id TEXT NOT NULL, host TEXT NOT NULL, \
            target TEXT NOT NULL, remount_policy TEXT, routes TEXT NOT NULL DEFAULT '[]', \
            enabled INTEGER NOT NULL DEFAULT 1, \
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')), \
            updated_at INTEGER NOT NULL DEFAULT 0, UNIQUE(host, name));";
        migrate_mounts_to_id_pk(&conn, new_sql).unwrap();

        assert!(cols(&conn, "mounts").contains(&"id".to_string()));
        let (id_len, name): (usize, String) = conn
            .query_row("SELECT length(id), name FROM mounts", [], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, String>(1)?))
            })
            .unwrap();
        assert!(id_len > 0, "id minted");
        assert_eq!(name, "data", "de-prefixed");
        // Idempotent: a second run is a no-op (id column already present).
        migrate_mounts_to_id_pk(&conn, new_sql).unwrap();
    }

    #[test]
    fn post_id_pk_mounts_table_gains_health_columns_so_merge_lands() {
        // A mounts table created at #250 (id PK) but before #252 added the
        // health/active_route columns. `migrate_mounts_to_id_pk` is a no-op (id
        // already present), so without the additive migration the table is stuck
        // missing `health` — a `SELECT *` reads Null for it ("expected text, got
        // Null", Bug B) and the replicated merge INSERT of the full column list
        // faults with "no column named health", so placements never land on the
        // peer (Bug C, mounts half).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE mounts (\
                id TEXT PRIMARY KEY, name TEXT NOT NULL, share_id TEXT NOT NULL, host TEXT NOT NULL, \
                target TEXT NOT NULL, remount_policy TEXT, routes TEXT NOT NULL DEFAULT '[]', \
                enabled INTEGER NOT NULL DEFAULT 1, \
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')), \
                updated_at INTEGER NOT NULL DEFAULT 0, UNIQUE(host, name));",
        )
        .unwrap();
        assert!(!cols(&conn, "mounts").contains(&"health".to_string()));

        // The exact reconcile the `f.name == "mounts"` branch runs.
        for (col, decl) in [
            ("remount_policy", "TEXT"),
            ("health", "TEXT NOT NULL DEFAULT 'missing'"),
            ("active_route", "TEXT"),
        ] {
            ensure_column(&conn, "mounts", col, decl).unwrap();
        }
        let after = cols(&conn, "mounts");
        assert!(after.contains(&"health".to_string()));
        assert!(after.contains(&"active_route".to_string()));

        // A replication-shaped INSERT with the full #252 column list now lands
        // (it faulted before the migration).
        conn.execute(
            "INSERT INTO mounts \
             (id, name, share_id, host, target, remount_policy, health, active_route, routes, enabled, updated_at) \
             VALUES ('id-peer','data','s1','baldur','/mnt/data',NULL,'missing',NULL,'[]',1,200)",
            [],
        )
        .unwrap();
        let health: String = conn
            .query_row("SELECT health FROM mounts WHERE id='id-peer'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(health, "missing");
    }

    #[test]
    fn legacy_shares_sources_column_is_retired_so_merge_insert_lands() {
        // A `shares` table that already gained a fresh empty `routes` but still
        // carries the stranded legacy `sources NOT NULL` column. The replication
        // merge INSERT omits `sources`, so without retiring it the controller's
        // share row (with its routes) faults the NOT-NULL constraint on the peer.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shares (\
                name TEXT PRIMARY KEY, id TEXT, backend TEXT, fstype TEXT, options TEXT, \
                options_rendered TEXT, credential TEXT, sources TEXT NOT NULL, \
                routes TEXT NOT NULL DEFAULT '[]', enabled INTEGER NOT NULL DEFAULT 1, \
                created_at TEXT, updated_at INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        // Both the rename (routes present → no-op) and the drop must run.
        rename_column_if_present(&conn, "shares", "sources", "routes").unwrap();
        drop_column_if_present(&conn, "shares", "sources").unwrap();
        let after = cols(&conn, "shares");
        assert!(
            !after.contains(&"sources".to_string()),
            "legacy sources retired"
        );
        assert!(after.contains(&"routes".to_string()));

        // A merge-shaped INSERT (no `sources`) now lands with the peer's routes.
        conn.execute(
            "INSERT INTO shares (name, id, backend, fstype, routes, enabled, updated_at) \
             VALUES ('data','s1','nfs','nfs4','[{\"kind\":\"lan_v4\",\"value\":\"10.10.10.10\"}]',1,200)",
            [],
        )
        .unwrap();
        let routes: String = conn
            .query_row("SELECT routes FROM shares WHERE name='data'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(routes.contains("10.10.10.10"), "authored routes replicated");
    }

    #[test]
    fn legacy_shares_sources_only_is_renamed_to_routes_preserving_column() {
        // The never-upgraded case: `sources` present, `routes` absent → rename
        // folds it onto `routes` (data preserved, format read as an empty typed
        // set until re-authored).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE shares (name TEXT PRIMARY KEY, sources TEXT NOT NULL);")
            .unwrap();
        rename_column_if_present(&conn, "shares", "sources", "routes").unwrap();
        drop_column_if_present(&conn, "shares", "sources").unwrap();
        let after = cols(&conn, "shares");
        assert!(after.contains(&"routes".to_string()));
        assert!(!after.contains(&"sources".to_string()));
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
