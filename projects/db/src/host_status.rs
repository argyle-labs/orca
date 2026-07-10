//! Per-peer system snapshot rows + age-based retention.
//!
//! See `migrations/20260517170000__host_status.up.sql` for the schema.
//!
//! Authority model:
//!   * Rows with `source='local'` are owned by the host whose peer_id matches.
//!     The local persistence task writes these every ~10 s.
//!   * Rows with `source='synced'` are mirrored from a peer's own DB by the
//!     pull-based sync task. They're read-only from this host's perspective.
//!
//! Retention: age-based by default (24 h). Configurable via the `config_store`
//! key `("host_status", "retention_days")`. A hard row-count cap guards against
//! unbounded growth if the retention setting is misconfigured.

use anyhow::Result;
use rusqlite::{Connection, params};

/// Hard row-count cap per peer. Safety guard independent of the age-based
/// retention policy. 8640 rows ≈ 24 h at one snapshot every 10 s.
pub const MAX_ROWS_PER_PEER: usize = 8640;

/// Default retention when no explicit config entry exists: 24 hours.
const DEFAULT_RETENTION_SECS: i64 = 86_400;

/// Default maximum total payload bytes per peer. None = no size cap.
/// Operators set a numeric override via `system.retention.set max_mb=…`.
const DEFAULT_MAX_BYTES: Option<i64> = None;

/// Default maximum row count per peer. Falls back to the safety guard
/// when no operator-set override exists.
const DEFAULT_MAX_ROWS: i64 = MAX_ROWS_PER_PEER as i64;

/// Per-peer retention policy resolved from `config_store` with peer-specific
/// override → global default → built-in default precedence. Returned by
/// `retention_for(peer_id)` so the sweeper can enforce all three caps in
/// a single pass.
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

fn resolve_per_peer_then_global<T>(
    conn: &Connection,
    noun: &str,
    knob: &str,
    peer_id: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let per_peer = crate::config_store::get(conn, noun, &format!("{knob}:{peer_id}"))
        .ok()
        .flatten()
        .and_then(|row| parse(&row.json));
    if per_peer.is_some() {
        return per_peer;
    }
    crate::config_store::get(conn, noun, knob)
        .ok()
        .flatten()
        .and_then(|row| parse(&row.json))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostStatusRow {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub payload_json: String,
    pub received_at_unix: i64,
    /// `"local"` (this host wrote it) or `"synced"` (mirrored from a peer).
    pub source: String,
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

/// Read the retention window in seconds for a given peer. Resolution order:
///   1. Per-peer override: config key `("host_status", "retention_days:<peer_id>")`
///   2. Global default:    config key `("host_status", "retention_days")`
///   3. [`DEFAULT_RETENTION_SECS`]
///
/// Per-system retention lets the UI keep, say, 7 days of mint but only 1 hour
/// of a noisy edge node.
pub fn retention_seconds(conn: &Connection, peer_id: &str) -> i64 {
    resolve_per_peer_then_global(
        conn,
        "host_status",
        "retention_days",
        peer_id,
        parse_retention_days,
    )
    .unwrap_or(DEFAULT_RETENTION_SECS)
}

/// Per-peer maximum total `payload_json` bytes. `None` = no size cap.
/// Set via `system.retention.set peer=<id> max_mb=<n>`.
pub fn retention_max_bytes(conn: &Connection, peer_id: &str) -> Option<i64> {
    resolve_per_peer_then_global(
        conn,
        "host_status",
        "retention_max_mb",
        peer_id,
        parse_mb_to_bytes,
    )
    .or(DEFAULT_MAX_BYTES)
}

/// Per-peer maximum row count. Falls back to the built-in safety cap.
pub fn retention_max_rows(conn: &Connection, peer_id: &str) -> i64 {
    resolve_per_peer_then_global(
        conn,
        "host_status",
        "retention_max_rows",
        peer_id,
        parse_i64_json,
    )
    .unwrap_or(DEFAULT_MAX_ROWS)
}

/// Resolve all three caps in one shot. The sweeper uses this so per-peer
/// enforcement happens against a consistent snapshot of the policy.
pub fn retention_for(conn: &Connection, peer_id: &str) -> RetentionPolicy {
    RetentionPolicy {
        age_secs: retention_seconds(conn, peer_id),
        max_bytes: retention_max_bytes(conn, peer_id),
        max_rows: retention_max_rows(conn, peer_id),
    }
}

/// Insert one snapshot, then prune the per-peer history:
///   1. Age-based: remove rows older than the configured retention window.
///   2. Count cap: keep at most [`MAX_ROWS_PER_PEER`] newest rows as a
///      safety guard against misconfigured retention.
///
/// Idempotent on `(peer_id, snapshot_at_unix)` — re-importing the same row
/// is a no-op (INSERT OR IGNORE).
pub fn insert_status(
    conn: &Connection,
    peer_id: &str,
    snapshot_at_unix: i64,
    payload_json: &str,
    received_at_unix: i64,
    source: &str,
) -> Result<bool> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO host_status
            (peer_id, snapshot_at_unix, payload_json, received_at_unix, source)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            peer_id,
            snapshot_at_unix,
            payload_json,
            received_at_unix,
            source
        ],
    )?;
    if inserted == 0 {
        return Ok(false);
    }
    // Age-based prune.
    let cutoff = utils::time::now().unix_seconds() - retention_seconds(conn, peer_id);
    conn.execute(
        "DELETE FROM host_status WHERE peer_id = ?1 AND snapshot_at_unix < ?2",
        params![peer_id, cutoff],
    )?;
    // Count-cap safety: keep at most MAX_ROWS_PER_PEER newest rows.
    conn.execute(
        "DELETE FROM host_status
         WHERE peer_id = ?1
           AND snapshot_at_unix < (
                SELECT MIN(snapshot_at_unix) FROM (
                    SELECT snapshot_at_unix FROM host_status
                    WHERE peer_id = ?1
                    ORDER BY snapshot_at_unix DESC
                    LIMIT ?2
                )
           )",
        params![peer_id, MAX_ROWS_PER_PEER as i64],
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

/// Enforce per-peer retention caps in one pass: age → size → count. Each
/// pass uses the policy resolved by [`retention_for`], so callers don't
/// need to thread three knobs through. Returns the number of rows deleted
/// by each policy axis.
///
/// `now_unix` is taken as a parameter so tests can pin time.
pub fn sweep_peer(conn: &Connection, peer_id: &str, now_unix: i64) -> Result<SweepReport> {
    let policy = retention_for(conn, peer_id);
    let mut report = SweepReport::default();

    // 1. Age cap.
    let cutoff = now_unix - policy.age_secs;
    let n = conn.execute(
        "DELETE FROM host_status WHERE peer_id = ?1 AND snapshot_at_unix < ?2",
        params![peer_id, cutoff],
    )?;
    report.deleted_by_age = n as u64;

    // 2. Size cap (optional). Walk newest→oldest, accumulate payload bytes,
    // delete everything past the cap. Done in SQL so the entire row set
    // isn't materialized in process memory.
    if let Some(max_bytes) = policy.max_bytes {
        let n = conn.execute(
            "DELETE FROM host_status
             WHERE peer_id = ?1 AND snapshot_at_unix IN (
                SELECT snapshot_at_unix FROM (
                    SELECT snapshot_at_unix,
                           SUM(length(payload_json)) OVER (
                               ORDER BY snapshot_at_unix DESC
                               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                           ) AS running_bytes
                    FROM host_status
                    WHERE peer_id = ?1
                ) WHERE running_bytes > ?2
             )",
            params![peer_id, max_bytes],
        )?;
        report.deleted_by_size = n as u64;
    }

    // 3. Row-count cap.
    let n = conn.execute(
        "DELETE FROM host_status
         WHERE peer_id = ?1
           AND snapshot_at_unix < (
                SELECT MIN(snapshot_at_unix) FROM (
                    SELECT snapshot_at_unix FROM host_status
                    WHERE peer_id = ?1
                    ORDER BY snapshot_at_unix DESC
                    LIMIT ?2
                )
           )",
        params![peer_id, policy.max_rows],
    )?;
    report.deleted_by_count = n as u64;

    Ok(report)
}

/// All peer_ids present in `host_status`. Used by the periodic sweeper to
/// iterate without depending on the pod peer table (sweep should still
/// work for peers that have departed but left rows behind).
pub fn distinct_peer_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT peer_id FROM host_status")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Sweep every peer present in the table. Returns the aggregate report.
pub fn sweep_all(conn: &Connection, now_unix: i64) -> Result<SweepReport> {
    let mut agg = SweepReport::default();
    for peer_id in distinct_peer_ids(conn)? {
        let r = sweep_peer(conn, &peer_id, now_unix)?;
        agg.deleted_by_age += r.deleted_by_age;
        agg.deleted_by_size += r.deleted_by_size;
        agg.deleted_by_count += r.deleted_by_count;
    }
    Ok(agg)
}

/// Operator-facing knobs persisted via `config_store`. Setting a knob to
/// `None` clears the per-peer override (falling back to the global default).
/// `peer_id = None` sets the global default itself.
pub fn set_retention_days(
    conn: &Connection,
    local_host: &str,
    peer_id: Option<&str>,
    days: Option<f64>,
) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_days",
        peer_id,
        days.map(|d| d.to_string()),
    )
}

pub fn set_retention_max_mb(
    conn: &Connection,
    local_host: &str,
    peer_id: Option<&str>,
    max_mb: Option<f64>,
) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_max_mb",
        peer_id,
        max_mb.map(|v| v.to_string()),
    )
}

pub fn set_retention_max_rows(
    conn: &Connection,
    local_host: &str,
    peer_id: Option<&str>,
    max_rows: Option<i64>,
) -> Result<()> {
    write_retention_knob(
        conn,
        local_host,
        "retention_max_rows",
        peer_id,
        max_rows.map(|v| v.to_string()),
    )
}

fn write_retention_knob(
    conn: &Connection,
    local_host: &str,
    knob: &str,
    peer_id: Option<&str>,
    value: Option<String>,
) -> Result<()> {
    let key = retention_config_key(knob, peer_id);
    match value {
        Some(v) => {
            crate::config_store::set(
                conn,
                local_host,
                local_host,
                "host_status",
                &key,
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
                &key,
                "system.retention.set",
            )?;
        }
    }
    Ok(())
}

fn retention_config_key(knob: &str, peer_id: Option<&str>) -> String {
    match peer_id {
        Some(p) => format!("{knob}:{p}"),
        None => knob.to_string(),
    }
}

/// Latest row for every peer present in the table. Used by the UI to render
/// the cross-mesh dashboard without an RPC fanout.
pub fn latest_per_peer(conn: &Connection) -> Result<Vec<HostStatusRow>> {
    let mut stmt = conn.prepare(
        "SELECT peer_id, snapshot_at_unix, payload_json, received_at_unix, source
         FROM host_status hs
         WHERE snapshot_at_unix = (
                SELECT MAX(snapshot_at_unix) FROM host_status
                WHERE peer_id = hs.peer_id
           )",
    )?;
    let rows = stmt
        .query_map([], row_to_status)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Rows for a single peer, optionally filtered by `since_unix` (exclusive).
/// Used by both the UI (history scrolling) and the sync puller (watermark
/// pull). Results are newest-first; cap with `limit` so a misbehaving caller
/// can't pull the entire history if it doesn't need to.
pub fn rows_for_peer(
    conn: &Connection,
    peer_id: &str,
    since_unix: Option<i64>,
    limit: usize,
) -> Result<Vec<HostStatusRow>> {
    let mut stmt = conn.prepare(
        "SELECT peer_id, snapshot_at_unix, payload_json, received_at_unix, source
         FROM host_status
         WHERE peer_id = ?1 AND snapshot_at_unix > ?2
         ORDER BY snapshot_at_unix DESC
         LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![peer_id, since_unix.unwrap_or(0), limit as i64],
            row_to_status,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Last snapshot timestamp recorded for `peer_id`. Used as the sync watermark
/// so the puller asks the peer only for rows it doesn't already have.
pub fn latest_snapshot_at(conn: &Connection, peer_id: &str) -> Result<Option<i64>> {
    let opt = conn
        .query_row(
            "SELECT MAX(snapshot_at_unix) FROM host_status WHERE peer_id = ?1",
            params![peer_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .unwrap_or(None);
    Ok(opt)
}

fn row_to_status(r: &rusqlite::Row<'_>) -> rusqlite::Result<HostStatusRow> {
    Ok(HostStatusRow {
        peer_id: r.get(0)?,
        snapshot_at_unix: r.get(1)?,
        payload_json: r.get(2)?,
        received_at_unix: r.get(3)?,
        source: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn as test_db;

    fn now() -> i64 {
        utils::time::now().unix_seconds()
    }

    #[test]
    fn insert_and_latest_per_peer() {
        let conn = test_db();
        let t = now();
        insert_status(&conn, "a", t - 200, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap();
        insert_status(&conn, "b", t - 150, "{}", t, "synced").unwrap();
        let rows = latest_per_peer(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r.peer_id == "a").unwrap();
        assert_eq!(a.snapshot_at_unix, t - 100);
    }

    #[test]
    fn insert_ignores_duplicate() {
        let conn = test_db();
        let t = now();
        assert!(insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap());
        assert!(!insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap());
    }

    #[test]
    fn prune_removes_rows_older_than_retention() {
        let conn = test_db();
        let t = now();
        // Two recent rows survive.
        insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 50, "{}", t, "local").unwrap();
        // Row older than 24 h gets pruned on the next insert.
        insert_status(&conn, "a", t - 90_001, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 10, "{}", t, "local").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM host_status WHERE peer_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // 3 recent rows remain; the old one was pruned.
        assert_eq!(n, 3);
    }

    #[test]
    fn rows_for_peer_respects_since() {
        let conn = test_db();
        let t = now();
        insert_status(&conn, "a", t - 300, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 200, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap();
        let rows = rows_for_peer(&conn, "a", Some(t - 250), 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].snapshot_at_unix, t - 100);
    }

    #[test]
    fn latest_snapshot_at_works() {
        let conn = test_db();
        assert_eq!(latest_snapshot_at(&conn, "a").unwrap(), None);
        let t = now();
        insert_status(&conn, "a", t - 300, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 100, "{}", t, "local").unwrap();
        insert_status(&conn, "a", t - 200, "{}", t, "local").unwrap();
        assert_eq!(latest_snapshot_at(&conn, "a").unwrap(), Some(t - 100));
    }
}
