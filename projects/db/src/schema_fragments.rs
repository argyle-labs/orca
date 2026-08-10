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
        }
    }
    // Reconcile the `mounts` columns UNCONDITIONALLY — outside the fragment loop.
    // The `mounts` SchemaFragment is a hand-written `inventory::submit!`
    // (mounts.rs), and unlike the macro-emitted endpoint fragments it can be
    // dead-stripped from the daemon binary, so the in-loop reconcile above may
    // never execute. #252 added `health`/`active_route`/`remount_policy` (and
    // this change adds the runtime `active_options`/`drift`) to the
    // CREATE, but `CREATE TABLE IF NOT EXISTS` is a no-op on an existing table,
    // so a pre-#252 `mounts` never gained them. On a DB with rows that makes
    // `SELECT *` read the missing NOT-NULL `health` as Null → every `mount.list`
    // 500s ("expected text, got Null"); empty peers hide it (no rows, no SELECT).
    // Running here (apply_fragments is always called) makes the columns land
    // regardless of whether the fragment was iterated. Idempotent; guarded by
    // table existence. The final backfill also repairs any `health` left Null by
    // an earlier path that added the column nullable.
    if table_exists(conn, "mounts")? {
        for (col, decl) in [
            ("remount_policy", "TEXT"),
            ("health", "TEXT NOT NULL DEFAULT 'missing'"),
            ("active_route", "TEXT"),
            ("active_options", "TEXT"),
            ("drift", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            ensure_column(conn, "mounts", col, decl)
                .map_err(|e| anyhow::anyhow!("mounts `{col}` unconditional reconcile: {e}"))?;
        }
        conn.execute_batch("UPDATE mounts SET health = 'missing' WHERE health IS NULL;")
            .map_err(|e| anyhow::anyhow!("mounts health null backfill: {e}"))?;
        // Mint an `id` for any `mounts` row missing one. `merge_table_natural`
        // (the mounts replication merge) inserts the incoming row's `id` verbatim
        // and never mints one, so a Null-`id` row on any peer — e.g. left by the
        // #250 per-host id-mint that missed rows — propagates fleet-wide. Since
        // `id` is read as a required TEXT (`from_dbrow`), a Null makes every
        // `storage.mount.list` 500 ("expected text, got Null"). Backfilling here
        // (via the registered `uuidv7()` SQL fn) repairs each host's own rows on
        // start, so its exports carry valid ids and the merge stops propagating
        // Nulls. Idempotent: the `id IS NULL OR id = ''` guard means a row that
        // already has an id is never re-minted.
        conn.execute_batch("UPDATE mounts SET id = uuidv7() WHERE id IS NULL OR id = '';")
            .map_err(|e| anyhow::anyhow!("mounts null-id backfill: {e}"))?;
    }
    // Retire the stranded legacy `shares.sources NOT NULL` column UNCONDITIONALLY,
    // for the same dead-strip reason as `mounts` above: the `shares`
    // SchemaFragment is macro-emitted (`endpoint_resource!`) via `inventory::submit!`
    // and can be stripped from the daemon binary, so the in-loop `routes TEXT`
    // reconcile that folds `sources` onto `routes` and drops it may never run.
    // `shares` (#132) shipped `sources TEXT NOT NULL`; the endpoint model renamed
    // it to the built-in `routes`, but a fleet DB that gained a fresh `routes`
    // kept the stranded `sources NOT NULL`. Its replication merge INSERT omits
    // `sources`, so it faults the NOT-NULL constraint (Error 1299) and a
    // controller-authored share (with its routes) never lands on peers — forcing
    // operators to push per-host with `--peer`. Fold any legacy `sources` onto
    // `routes`, ensure `routes` exists, then drop the leftover `sources`.
    // Idempotent; guarded by table existence.
    if table_exists(conn, "shares")? {
        rename_column_if_present(conn, "shares", "sources", "routes")
            .map_err(|e| anyhow::anyhow!("shares sources→routes unconditional rename: {e}"))?;
        ensure_column(conn, "shares", "routes", "TEXT NOT NULL DEFAULT '[]'")
            .map_err(|e| anyhow::anyhow!("shares routes unconditional reconcile: {e}"))?;
        drop_column_if_present(conn, "shares", "sources")
            .map_err(|e| anyhow::anyhow!("shares drop legacy sources unconditional: {e}"))?;
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
    fn apply_fragments_reconciles_mounts_columns_even_when_fragment_not_iterated() {
        // A mounts table created at #250 (id PK) but before #252 added the
        // health/active_route columns, WITH an existing row. The `mounts`
        // SchemaFragment is a hand-written `inventory::submit!` that can be
        // dead-stripped from the binary — and it is NOT registered in this db-crate
        // test's inventory at all, exactly mirroring the stripped-daemon case. So
        // the in-loop reconcile never runs; only the UNCONDITIONAL post-loop block
        // can add the columns. Without it, `SELECT *` reads the missing NOT-NULL
        // `health` as Null → every `mount.list` 500s ("expected text, got Null").
        let conn = Connection::open_in_memory().unwrap();
        crate::register_sql_functions(&conn).unwrap(); // uuidv7() for the id backfill
        conn.execute_batch(
            "CREATE TABLE mounts (\
                id TEXT PRIMARY KEY, name TEXT NOT NULL, share_id TEXT NOT NULL, host TEXT NOT NULL, \
                target TEXT NOT NULL, remount_policy TEXT, routes TEXT NOT NULL DEFAULT '[]', \
                enabled INTEGER NOT NULL DEFAULT 1, \
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')), \
                updated_at INTEGER NOT NULL DEFAULT 0, UNIQUE(host, name));",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mounts (id, name, share_id, host, target, routes, enabled, updated_at) \
             VALUES ('id-existing','data','s1','baldur','/mnt/data','[]',1,100)",
            [],
        )
        .unwrap();
        assert!(!cols(&conn, "mounts").contains(&"health".to_string()));

        // The real entry point — NOT the in-loop branch — must land the columns.
        apply_fragments(&conn).unwrap();

        let after = cols(&conn, "mounts");
        assert!(after.contains(&"health".to_string()));
        assert!(after.contains(&"active_route".to_string()));
        assert!(after.contains(&"remount_policy".to_string()));
        // The pre-existing row now reads a concrete health (ADD COLUMN backfill),
        // so `mount.list`'s `SELECT *` no longer faults.
        let health: String = conn
            .query_row(
                "SELECT health FROM mounts WHERE id='id-existing'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(health, "missing");
    }

    #[test]
    fn apply_fragments_backfills_nullable_health_left_by_an_earlier_path() {
        // A `mounts` table where `health` exists but is NULLABLE and holds Null
        // (e.g. added by a merge/export path, not the NOT-NULL reconcile).
        // `ensure_column` sees the column present and skips it, so only the
        // explicit backfill repairs the Null → 'missing'.
        let conn = Connection::open_in_memory().unwrap();
        crate::register_sql_functions(&conn).unwrap(); // uuidv7() for the id backfill
        conn.execute_batch(
            "CREATE TABLE mounts (\
                id TEXT PRIMARY KEY, name TEXT NOT NULL, share_id TEXT NOT NULL, host TEXT NOT NULL, \
                target TEXT NOT NULL, remount_policy TEXT, health TEXT, active_route TEXT, \
                routes TEXT NOT NULL DEFAULT '[]', enabled INTEGER NOT NULL DEFAULT 1, \
                updated_at INTEGER NOT NULL DEFAULT 0, UNIQUE(host, name));",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mounts (id, name, share_id, host, target, health, routes, enabled, updated_at) \
             VALUES ('id-null','data','s1','baldur','/mnt/data',NULL,'[]',1,100)",
            [],
        )
        .unwrap();

        apply_fragments(&conn).unwrap();

        let health: Option<String> = conn
            .query_row("SELECT health FROM mounts WHERE id='id-null'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(health.as_deref(), Some("missing"));
    }

    #[test]
    fn apply_fragments_mints_ids_for_null_id_mount_rows() {
        // The mounts replication merge (`merge_table_natural`) inserts the incoming
        // row's `id` verbatim and never mints one, so a Null-`id` row on any peer
        // propagates fleet-wide. `from_dbrow` reads `id` as a required TEXT, so a
        // Null makes every `storage.mount.list` 500 ("expected text, got Null").
        // apply_fragments must mint an id for such rows via `uuidv7()`.
        let conn = Connection::open_in_memory().unwrap();
        crate::register_sql_functions(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE mounts (\
                id TEXT PRIMARY KEY, name TEXT NOT NULL, share_id TEXT NOT NULL, host TEXT NOT NULL, \
                target TEXT NOT NULL, remount_policy TEXT, health TEXT NOT NULL DEFAULT 'missing', \
                active_route TEXT, routes TEXT NOT NULL DEFAULT '[]', enabled INTEGER NOT NULL DEFAULT 1, \
                updated_at INTEGER NOT NULL DEFAULT 0, UNIQUE(host, name));",
        )
        .unwrap();
        // A Null-id row and an empty-string-id row — both must be repaired.
        conn.execute(
            "INSERT INTO mounts (id, name, share_id, host, target, routes, enabled, updated_at) \
             VALUES (NULL,'data','s1','frigg','/mnt/data','[]',1,100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mounts (id, name, share_id, host, target, routes, enabled, updated_at) \
             VALUES ('','backups','s2','loki','/mnt/backups','[]',1,100)",
            [],
        )
        .unwrap();
        assert_eq!(
            2i64,
            conn.query_row(
                "SELECT COUNT(*) FROM mounts WHERE id IS NULL OR id=''",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
        );

        apply_fragments(&conn).unwrap();

        // No row is left without a usable id, and each got a distinct valid one.
        let bad: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mounts WHERE id IS NULL OR id=''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "every mount row has a minted id");
        let distinct: i64 = conn
            .query_row("SELECT COUNT(DISTINCT id) FROM mounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(distinct, 2, "minted ids are distinct per row");
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
    fn apply_fragments_retires_shares_sources_even_when_fragment_not_iterated() {
        // The live controller→peer bug: the `shares` SchemaFragment is macro-emitted
        // via `inventory::submit!` and can be dead-stripped from the daemon binary —
        // and it is NOT registered in this db-crate test's inventory at all, exactly
        // mirroring the stripped-daemon case. So the in-loop `routes TEXT` reconcile
        // that drops `sources` never runs; only the UNCONDITIONAL post-loop block
        // can retire it. Without it, the replication merge INSERT (which omits
        // `sources`) faults `NOT NULL constraint failed: shares.sources` (Error 1299)
        // and share config never propagates.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shares (\
                name TEXT PRIMARY KEY, id TEXT, backend TEXT, fstype TEXT, options TEXT, \
                options_rendered TEXT, credential TEXT, sources TEXT NOT NULL, \
                routes TEXT NOT NULL DEFAULT '[]', enabled INTEGER NOT NULL DEFAULT 1, \
                created_at TEXT, updated_at INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        assert!(cols(&conn, "shares").contains(&"sources".to_string()));

        // The real entry point — NOT the in-loop branch — must retire `sources`.
        apply_fragments(&conn).unwrap();

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
