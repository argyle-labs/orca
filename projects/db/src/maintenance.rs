//! Database maintenance — size visibility, retention sweeps, vacuum.
//!
//! All operations take an already-keyed `Connection`. The tool surfaces
//! live in `system::db_admin` so CLI / REST / MCP share one path
//! ([[feedback-cli-api-mcp-one-path]]).
//!
//! ## auto_vacuum
//!
//! `PRAGMA auto_vacuum = INCREMENTAL` is set in `apply_tuning_pragmas`
//! ([[lib.rs:147]]). On an EXISTING database the pragma is recorded but
//! does NOT take effect until a full `VACUUM` runs once — after that,
//! `incremental_vacuum(N)` can reclaim N pages without rewriting the
//! whole file. New databases pick up incremental auto-vacuum immediately.
//!
//! ## Retention
//!
//! `sweep_session_events` deletes by ISO-8601 timestamp comparison. The
//! `session_events_fts` FTS5 mirror cascades automatically via the
//! `se_fts_delete` trigger ([[lib.rs:624]]).

use anyhow::{Context, Result};
use rusqlite::Connection;

/// One row of [`table_stats`] output: storage bytes + live row count.
#[derive(Debug, Clone)]
pub struct TableStat {
    pub name: String,
    pub bytes: i64,
    pub rows: i64,
}

/// Per-table storage cost via the `dbstat` virtual table.
///
/// `dbstat` is enabled at build time via `SQLITE_ENABLE_DBSTAT_VTAB`
/// (`.cargo/config.toml`). `aggregate='leaf'` restricts the scan to leaf
/// pages, which gives the actual table data size (interior B-tree pages
/// belong to the table too but are tiny; including them changes nothing
/// at the order-of-magnitude scale we care about).
///
/// Row counts use a per-table `SELECT COUNT(*)` — slower than `dbstat`'s
/// page math but accurate. Tolerated because this is an admin-tool
/// query, not a hot path.
pub fn table_stats(conn: &Connection) -> Result<Vec<TableStat>> {
    let mut byte_stmt = conn
        .prepare(
            "SELECT name, SUM(pgsize) AS bytes
             FROM dbstat
             WHERE aggregate = 1
             GROUP BY name
             ORDER BY bytes DESC",
        )
        .context("prepare dbstat scan")?;
    let byte_rows = byte_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .context("run dbstat scan")?;

    let mut out = Vec::new();
    for row in byte_rows {
        let (name, bytes) = row.context("read dbstat row")?;
        // Skip sqlite's internal shadow tables — confuses operators and
        // they can't act on them anyway.
        if name.starts_with("sqlite_") {
            continue;
        }
        // FTS shadow tables (e.g. session_events_fts_data) are valid
        // storage consumers — keep them so the operator can see them.
        let rows = row_count(conn, &name).unwrap_or(-1);
        out.push(TableStat { name, bytes, rows });
    }
    Ok(out)
}

fn row_count(conn: &Connection, table: &str) -> Result<i64> {
    // Identifier must be quoted to handle table names with reserved
    // words / dots. `dbstat` only returns names from `sqlite_master`,
    // which are valid identifiers, so the double-quote wrap is safe.
    let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
    conn.query_row(&sql, [], |r| r.get::<_, i64>(0))
        .with_context(|| format!("row count for {table}"))
}

/// Delete `session_events` older than `days` days. Returns the number of
/// rows removed. FTS5 mirror cascades via the `se_fts_delete` trigger.
///
/// Uses ISO-8601 string comparison — safe because `timestamp` is written
/// with `strftime('%Y-%m-%dT%H:%M:%SZ', 'now')`, which lexicographically
/// orders the same as chronologically.
pub fn sweep_session_events(conn: &Connection, days: u32) -> Result<u64> {
    let cutoff_expr = format!("strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{days} days')");
    let sql = format!("DELETE FROM session_events WHERE timestamp < {cutoff_expr}");
    let n = conn.execute(&sql, []).context("sweep session_events")?;
    Ok(n as u64)
}

/// Reclaim `pages` worth of freed space back to the filesystem without
/// rewriting the whole file. Cheap and incremental — safe to call
/// periodically. Requires `auto_vacuum = INCREMENTAL` to have been
/// active when the freed pages were created (new DBs only, OR existing
/// DBs that have had a full `vacuum()` run once after the pragma was
/// added).
pub fn incremental_vacuum(conn: &Connection, pages: u32) -> Result<()> {
    let sql = format!("PRAGMA incremental_vacuum({pages})");
    conn.execute_batch(&sql).context("incremental_vacuum")?;
    Ok(())
}

/// Full VACUUM — rewrites the entire database file, reclaiming all
/// unused space. EXPENSIVE: takes a write lock for the duration and
/// requires ~2× the database size in temporary disk. Operator-driven
/// only; never run on a hot path. Also serves as the one-shot needed to
/// activate `auto_vacuum = INCREMENTAL` on an existing database.
pub fn vacuum(conn: &Connection) -> Result<()> {
    conn.execute_batch("VACUUM").context("vacuum")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn table_stats_returns_known_tables() {
        let conn = test_conn();
        let stats = table_stats(&conn).expect("table_stats ok");
        // Migrations always create the apply_log + at least one user
        // table; at minimum we expect non-empty output and no panics.
        assert!(!stats.is_empty(), "expected at least one table in dbstat");
        // No sqlite_internal rows should leak through.
        assert!(stats.iter().all(|s| !s.name.starts_with("sqlite_")));
    }

    #[test]
    fn sweep_session_events_with_no_rows_returns_zero() {
        let conn = test_conn();
        let n = sweep_session_events(&conn, 14).expect("sweep ok");
        assert_eq!(n, 0);
    }

    #[test]
    fn sweep_session_events_removes_only_old_rows() {
        let conn = test_conn();
        // Two rows: one ancient, one fresh.
        conn.execute(
            "INSERT INTO session_events (id, session, timestamp, content)
             VALUES ('old', 's', '2020-01-01T00:00:00Z', 'ancient'),
                    ('new', 's', strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), 'fresh')",
            [],
        )
        .unwrap();
        let n = sweep_session_events(&conn, 14).expect("sweep ok");
        assert_eq!(n, 1);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        // FTS mirror cascaded.
        let fts: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_events_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts, 1);
    }

    #[test]
    fn incremental_vacuum_and_vacuum_no_op_on_empty() {
        let conn = test_conn();
        incremental_vacuum(&conn, 1).expect("incremental_vacuum ok");
        vacuum(&conn).expect("vacuum ok");
    }
}
