//! This host's own system-snapshot timeseries + age-based retention.
//!
//! See `migrations/20260731000000__host_status_single_host.up.sql` for the
//! schema. `host_status` holds only THIS host's own rows — telemetry is
//! local-only and fetched on demand (`pod::peer_info`); it is never mirrored
//! across the mesh. The local persistence task writes one row per cadence tick.
//!
//! Retention: age-based by default (24 h). Configurable via the `config_store`
//! key `("host_status", "retention_days")`. A hard row-count cap guards against
//! unbounded growth if the retention setting is misconfigured.

use anyhow::Result;
use rusqlite::{Connection, params};

/// Hard row-count cap. Safety guard independent of the age-based retention
/// policy. 2880 rows ≈ 24 h at one snapshot every 30 s (the idle
/// SLOW_CADENCE); the FAST_CADENCE (~2 s, only while a UI is subscribed)
/// trades age for count but the byte cap below is the real backstop.
pub const MAX_ROWS: usize = 2880;

/// Default retention when no explicit config entry exists: 24 hours.
const DEFAULT_RETENTION_SECS: i64 = 86_400;

/// Default maximum total payload bytes. A size cap MUST exist by default:
/// without one, a large per-row payload (the snapshot embeds a history ring)
/// multiplied by the row cap balloons the DB to multiple GB (observed:
/// 3.7 GB/host, one `host_status` table). 25 MiB is generous for the slim
/// persisted rows yet bounds the worst case. Operators override via
/// `system.retention.set max_mb=…`.
const DEFAULT_MAX_BYTES: Option<i64> = Some(25 * 1024 * 1024);

/// Default maximum row count. Falls back to the safety guard when no
/// operator-set override exists.
const DEFAULT_MAX_ROWS: i64 = MAX_ROWS as i64;

/// Retention policy resolved from `config_store` with global override →
/// built-in default precedence. Returned by `retention_for` so the sweeper
/// can enforce all three caps in a single pass.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Age cap in seconds. Rows older than `now - age_secs` are deleted.
    pub age_secs: i64,
    /// Optional size cap. When set, oldest rows are deleted until the
    /// sum of `length(payload_json)` is at or below this value.
    pub max_bytes: Option<i64>,
    /// Hard row-count cap. Rows beyond the newest `max_rows` are deleted.
    pub max_rows: i64,
}

fn parse_i64_json(json: &str) -> Option<i64> {
    json.trim_matches('"')
        .parse::<i64>()
        .ok()
        .filter(|&v| v >= 0)
}

fn parse_mb_to_bytes(json: &str) -> Option<i64> {
    json.trim_matches('"')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|mb| (mb * 1_048_576.0) as i64)
}

fn resolve_global<T>(
    conn: &Connection,
    noun: &str,
    knob: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    crate::config_store::get(conn, noun, knob)
        .ok()
        .flatten()
        .and_then(|row| parse(&row.json))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostStatusRow {
    pub snapshot_at_unix: i64,
    pub payload_json: String,
    pub received_at_unix: i64,
}

/// Parse a `retention_days` config row into a clamped seconds window.
/// 0 = "no history"; negative is invalid and yields `None` (fall through).
fn parse_retention_days(json: &str) -> Option<i64> {
    json.trim_matches('"')
        .parse::<f64>()
        .ok()
        .map(|days| (days * 86_400.0) as i64)
        .filter(|&s| s >= 0)
}

/// Read the retention window in seconds. Resolution order:
///   1. Global override: config key `("host_status", "retention_days")`
///   2. [`DEFAULT_RETENTION_SECS`]
pub fn retention_seconds(conn: &Connection) -> i64 {
    resolve_global(conn, "host_status", "retention_days", parse_retention_days)
        .unwrap_or(DEFAULT_RETENTION_SECS)
}

/// Maximum total `payload_json` bytes. `None` = no size cap.
/// Set via `system.retention.set max_mb=<n>`.
pub fn retention_max_bytes(conn: &Connection) -> Option<i64> {
    resolve_global(conn, "host_status", "retention_max_mb", parse_mb_to_bytes).or(DEFAULT_MAX_BYTES)
}

/// Maximum row count. Falls back to the built-in safety cap.
pub fn retention_max_rows(conn: &Connection) -> i64 {
    resolve_global(conn, "host_status", "retention_max_rows", parse_i64_json)
        .unwrap_or(DEFAULT_MAX_ROWS)
}

/// Resolve all three caps in one shot. The sweeper uses this so enforcement
/// happens against a consistent snapshot of the policy.
pub fn retention_for(conn: &Connection) -> RetentionPolicy {
    RetentionPolicy {
        age_secs: retention_seconds(conn),
        max_bytes: retention_max_bytes(conn),
        max_rows: retention_max_rows(conn),
    }
}

/// Insert one snapshot, then prune this host's history:
///   1. Age-based: remove rows older than `age_secs` (the configured retention
///      window, resolved from orca.db config by the caller).
///   2. Count cap: keep at most [`MAX_ROWS`] newest rows as a safety guard
///      against misconfigured retention.
///
/// `conn` is the encrypted `metrics.db` connection — this timeseries lives there,
/// not in orca.db (config only). Idempotent on `snapshot_at_unix` — re-inserting
/// the same row is a no-op (INSERT OR IGNORE).
pub fn insert_status(
    conn: &Connection,
    snapshot_at_unix: i64,
    payload_json: &str,
    received_at_unix: i64,
    age_secs: i64,
) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO host_status
            (snapshot_at_unix, payload_json, received_at_unix)
         VALUES (?1, ?2, ?3)",
        params![snapshot_at_unix, payload_json, received_at_unix],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    // Age-based prune.
    let cutoff = utils::time::now().unix_seconds() - age_secs;
    conn.execute(
        "DELETE FROM host_status WHERE snapshot_at_unix < ?1",
        params![cutoff],
    )?;
    // Count-cap safety: keep at most MAX_ROWS newest rows.
    conn.execute(
        "DELETE FROM host_status
         WHERE snapshot_at_unix < (
                SELECT MIN(snapshot_at_unix) FROM (
                    SELECT snapshot_at_unix FROM host_status
                    ORDER BY snapshot_at_unix DESC
                    LIMIT ?1
                )
           )",
        params![MAX_ROWS as i64],
    )?;
    Ok(true)
}

/// Rows deleted by a single sweep pass. Returned so the caller can log
/// + emit a structured event.
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepReport {
    pub deleted_by_age: u64,
    pub deleted_by_size: u64,
    pub deleted_by_count: u64,
}

impl SweepReport {
    pub fn total(&self) -> u64 {
        self.deleted_by_age + self.deleted_by_size + self.deleted_by_count
    }
}

/// Enforce retention caps in one pass: age → size → count. Returns the number of
/// rows deleted by each policy axis.
///
/// `conn` is the `metrics.db` connection (where the timeseries lives); `policy`
/// is resolved from orca.db config by the caller ([`retention_for`]). `now_unix`
/// is a parameter so tests can pin time.
pub fn sweep(conn: &Connection, policy: RetentionPolicy, now_unix: i64) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    // 1. Age cap.
    let cutoff = now_unix - policy.age_secs;
    let n = conn.execute(
        "DELETE FROM host_status WHERE snapshot_at_unix < ?1",
        params![cutoff],
    )?;
    report.deleted_by_age = n as u64;

    // 2. Size cap (optional). Walk newest→oldest, accumulate payload bytes,
    // delete everything past the cap. Done in SQL so the entire row set
    // isn't materialized in process memory.
    if let Some(max_bytes) = policy.max_bytes {
        let n = conn.execute(
            "DELETE FROM host_status
             WHERE snapshot_at_unix IN (
                SELECT snapshot_at_unix FROM (
                    SELECT snapshot_at_unix,
                           SUM(length(payload_json)) OVER (
                               ORDER BY snapshot_at_unix DESC
                               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                           ) AS running_bytes
                    FROM host_status
                ) WHERE running_bytes > ?1
             )",
            params![max_bytes],
        )?;
        report.deleted_by_size = n as u64;
    }

    // 3. Row-count cap.
    let n = conn.execute(
        "DELETE FROM host_status
         WHERE snapshot_at_unix < (
                SELECT MIN(snapshot_at_unix) FROM (
                    SELECT snapshot_at_unix FROM host_status
                    ORDER BY snapshot_at_unix DESC
                    LIMIT ?1
                )
           )",
        params![policy.max_rows],
    )?;
    report.deleted_by_count = n as u64;

    Ok(report)
}

/// Operator-facing knobs persisted via `config_store`. Setting a knob to
/// `None` clears the override (falling back to the built-in default).
pub fn set_retention_days(conn: &Connection, local_host: &str, days: Option<f64>) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_days",
        days.map(|d| d.to_string()),
    )
}

pub fn set_retention_max_mb(
    conn: &Connection,
    local_host: &str,
    max_mb: Option<f64>,
) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_max_mb",
        max_mb.map(|v| v.to_string()),
    )
}

pub fn set_retention_max_rows(
    conn: &Connection,
    local_host: &str,
    max_rows: Option<i64>,
) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_max_rows",
        max_rows.map(|v| v.to_string()),
    )
}

fn write_retention_knob(
    conn: &Connection,
    local_host: &str,
    knob: &str,
    value: Option<String>,
) -> Result<()> {
    match value {
        Some(v) => {
            crate::config_store::set(
                conn,
                local_host,
                local_host,
                "host_status",
                knob,
                &v,
                "system.retention.set",
            )?;
        }
        None => {
            crate::config_store::delete(
                conn,
                local_host,
                local_host,
                "host_status",
                knob,
                "system.retention.set",
            )?;
        }
    }
    Ok(())
}

/// The single newest row, or `None` if the table is empty. Used by `pod.list`
/// to enrich this host's member row with its latest `system` snapshot.
pub fn latest(conn: &Connection) -> Result<Option<HostStatusRow>> {
    let opt = conn
        .query_row(
            "SELECT snapshot_at_unix, payload_json, received_at_unix
             FROM host_status
             ORDER BY snapshot_at_unix DESC
             LIMIT 1",
            [],
            row_to_status,
        )
        .ok();
    Ok(opt)
}

/// This host's rows, optionally filtered by `since_unix` (exclusive). Used by
/// the UI history scrolling. Results are newest-first; cap with `limit` so a
/// misbehaving caller can't pull the entire history if it doesn't need to.
pub fn rows_since(
    conn: &Connection,
    since_unix: Option<i64>,
    limit: usize,
) -> Result<Vec<HostStatusRow>> {
    let mut stmt = conn.prepare(
        "SELECT snapshot_at_unix, payload_json, received_at_unix
         FROM host_status
         WHERE snapshot_at_unix > ?1
         ORDER BY snapshot_at_unix DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(
            params![since_unix.unwrap_or(0), limit as i64],
            row_to_status,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_to_status(r: &rusqlite::Row<'_>) -> rusqlite::Result<HostStatusRow> {
    Ok(HostStatusRow {
        snapshot_at_unix: r.get(0)?,
        payload_json: r.get(1)?,
        received_at_unix: r.get(2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn as test_db;

    fn now() -> i64 {
        utils::time::now().unix_seconds()
    }

    /// A throwaway `metrics.db`-shaped connection carrying the `host_status`
    /// table — where the timeseries lives. Data ops (`insert_status`, `sweep`,
    /// `latest`, `rows_since`) run against this; retention config resolution
    /// (`retention_for` and friends) runs against an orca.db [`test_db`].
    fn metrics_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        crate::metrics::init_schema(&conn).expect("init metrics schema");
        conn
    }

    /// A generous per-insert age window so a row survives `insert_status`'s own
    /// age-prune; sweep tests then apply a tighter explicit policy.
    const KEEP_ALL_AGE: i64 = 1000 * 86_400;

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM host_status", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn insert_and_latest() {
        let conn = metrics_db();
        let t = now();
        insert_status(&conn, t - 200, "{}", t, KEEP_ALL_AGE).unwrap();
        insert_status(&conn, t - 100, "{}", t, KEEP_ALL_AGE).unwrap();
        let latest = latest(&conn).unwrap().unwrap();
        assert_eq!(latest.snapshot_at_unix, t - 100);
    }

    #[test]
    fn latest_is_none_when_empty() {
        let conn = metrics_db();
        assert!(latest(&conn).unwrap().is_none());
    }

    #[test]
    fn insert_ignores_duplicate() {
        let conn = metrics_db();
        let t = now();
        assert!(insert_status(&conn, t - 100, "{}", t, KEEP_ALL_AGE).unwrap());
        assert!(!insert_status(&conn, t - 100, "{}", t, KEEP_ALL_AGE).unwrap());
    }

    #[test]
    fn prune_removes_rows_older_than_retention() {
        let conn = metrics_db();
        let t = now();
        // Two recent rows survive a 24 h window.
        insert_status(&conn, t - 100, "{}", t, 86_400).unwrap();
        insert_status(&conn, t - 50, "{}", t, 86_400).unwrap();
        // Row older than 24 h gets pruned on the next insert.
        insert_status(&conn, t - 90_001, "{}", t, 86_400).unwrap();
        insert_status(&conn, t - 10, "{}", t, 86_400).unwrap();
        // 3 recent rows remain; the old one was pruned.
        assert_eq!(count(&conn), 3);
    }

    #[test]
    fn rows_since_respects_since() {
        let conn = metrics_db();
        let t = now();
        insert_status(&conn, t - 300, "{}", t, KEEP_ALL_AGE).unwrap();
        insert_status(&conn, t - 200, "{}", t, KEEP_ALL_AGE).unwrap();
        insert_status(&conn, t - 100, "{}", t, KEEP_ALL_AGE).unwrap();
        let rows = rows_since(&conn, Some(t - 250), 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn parse_retention_days_variants() {
        assert_eq!(parse_retention_days("1"), Some(86_400));
        assert_eq!(parse_retention_days("\"2\""), Some(172_800));
        assert_eq!(parse_retention_days("0"), Some(0));
        assert_eq!(parse_retention_days("0.5"), Some(43_200));
        assert_eq!(parse_retention_days("-1"), None);
        assert_eq!(parse_retention_days("nope"), None);
    }

    #[test]
    fn parse_i64_json_variants() {
        assert_eq!(parse_i64_json("100"), Some(100));
        assert_eq!(parse_i64_json("\"42\""), Some(42));
        assert_eq!(parse_i64_json("0"), Some(0));
        assert_eq!(parse_i64_json("-5"), None);
        assert_eq!(parse_i64_json("1.5"), None);
        assert_eq!(parse_i64_json("x"), None);
    }

    #[test]
    fn parse_mb_to_bytes_variants() {
        assert_eq!(parse_mb_to_bytes("1"), Some(1_048_576));
        assert_eq!(parse_mb_to_bytes("\"2\""), Some(2_097_152));
        assert_eq!(parse_mb_to_bytes("0"), Some(0));
        assert_eq!(parse_mb_to_bytes("-1"), None);
        assert_eq!(parse_mb_to_bytes("inf"), None);
        assert_eq!(parse_mb_to_bytes("bad"), None);
    }

    #[test]
    fn retention_defaults_when_unset() {
        let conn = test_db();
        assert_eq!(retention_seconds(&conn), DEFAULT_RETENTION_SECS);
        assert_eq!(retention_max_bytes(&conn), DEFAULT_MAX_BYTES);
        assert_eq!(retention_max_rows(&conn), DEFAULT_MAX_ROWS);
    }

    #[test]
    fn set_retention_days_overrides_default() {
        let conn = test_db();
        set_retention_days(&conn, "host", Some(7.0)).unwrap();
        assert_eq!(retention_seconds(&conn), 7 * 86_400);
        // Clearing the override falls back to the built-in default.
        set_retention_days(&conn, "host", None).unwrap();
        assert_eq!(retention_seconds(&conn), DEFAULT_RETENTION_SECS);
    }

    #[test]
    fn set_retention_max_mb_and_rows() {
        let conn = test_db();
        set_retention_max_mb(&conn, "host", Some(3.0)).unwrap();
        assert_eq!(retention_max_bytes(&conn), Some(3 * 1_048_576));
        set_retention_max_rows(&conn, "host", Some(50)).unwrap();
        assert_eq!(retention_max_rows(&conn), 50);
    }

    #[test]
    fn retention_for_bundles_three_caps() {
        let conn = test_db();
        set_retention_days(&conn, "host", Some(2.0)).unwrap();
        set_retention_max_mb(&conn, "host", Some(1.0)).unwrap();
        set_retention_max_rows(&conn, "host", Some(99)).unwrap();
        let p = retention_for(&conn);
        assert_eq!(p.age_secs, 2 * 86_400);
        assert_eq!(p.max_bytes, Some(1_048_576));
        assert_eq!(p.max_rows, 99);
    }

    #[test]
    fn sweep_report_total_sums_axes() {
        let r = SweepReport {
            deleted_by_age: 1,
            deleted_by_size: 2,
            deleted_by_count: 3,
        };
        assert_eq!(r.total(), 6);
    }

    #[test]
    fn sweep_age_cap() {
        let conn = metrics_db();
        // Insert under a generous window so the old row survives insert_status's
        // own age-prune; then sweep with a 1-day policy.
        let t = now();
        insert_status(&conn, t - 10, "{}", t, KEEP_ALL_AGE).unwrap();
        insert_status(&conn, t - 200_000, "{}", t, KEEP_ALL_AGE).unwrap();
        let policy = RetentionPolicy {
            age_secs: 86_400,
            max_bytes: DEFAULT_MAX_BYTES,
            max_rows: DEFAULT_MAX_ROWS,
        };
        let report = sweep(&conn, policy, t).unwrap();
        assert_eq!(report.deleted_by_age, 1);
        assert_eq!(count(&conn), 1);
    }

    #[test]
    fn sweep_row_count_cap() {
        let conn = metrics_db();
        let t = now();
        for i in 0..5 {
            insert_status(&conn, t - 5 + i, "{}", t, KEEP_ALL_AGE).unwrap();
        }
        let policy = RetentionPolicy {
            age_secs: DEFAULT_RETENTION_SECS,
            max_bytes: DEFAULT_MAX_BYTES,
            max_rows: 2,
        };
        let report = sweep(&conn, policy, t + 10).unwrap();
        assert_eq!(report.deleted_by_count, 3);
        assert_eq!(count(&conn), 2);
    }

    #[test]
    fn sweep_size_cap() {
        let conn = metrics_db();
        // Each payload is 10 bytes; cap at ~15 bytes keeps only the newest.
        let t = now();
        insert_status(&conn, t - 20, "0123456789", t, KEEP_ALL_AGE).unwrap();
        insert_status(&conn, t - 10, "0123456789", t, KEEP_ALL_AGE).unwrap();
        let policy = RetentionPolicy {
            age_secs: DEFAULT_RETENTION_SECS,
            max_bytes: Some(15),
            max_rows: DEFAULT_MAX_ROWS,
        };
        let report = sweep(&conn, policy, t + 100).unwrap();
        assert_eq!(report.deleted_by_size, 1);
        assert_eq!(latest(&conn).unwrap().unwrap().snapshot_at_unix, t - 10);
    }

    #[test]
    fn host_status_row_serde_round_trip() {
        let row = HostStatusRow {
            snapshot_at_unix: 100,
            payload_json: "{\"k\":1}".into(),
            received_at_unix: 200,
        };
        let json = serde_json::to_string(&row).unwrap();
        let back: HostStatusRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back.snapshot_at_unix, 100);
        assert_eq!(back.payload_json, "{\"k\":1}");
        assert_eq!(back.received_at_unix, 200);
    }

    #[test]
    fn rows_since_reads_all_fields() {
        let conn = metrics_db();
        let t = now();
        insert_status(&conn, t - 100, "{\"x\":1}", t, KEEP_ALL_AGE).unwrap();
        let rows = rows_since(&conn, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload_json, "{\"x\":1}");
        assert_eq!(rows[0].received_at_unix, t);
    }
}
