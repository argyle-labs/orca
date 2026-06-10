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
    let per_peer =
        crate::config_store::get(conn, "host_status", &format!("retention_days:{peer_id}"))
            .ok()
            .flatten()
            .and_then(|row| parse_retention_days(&row.json));
    if let Some(secs) = per_peer {
        return secs;
    }
    crate::config_store::get(conn, "host_status", "retention_days")
        .ok()
        .flatten()
        .and_then(|row| parse_retention_days(&row.json))
        .unwrap_or(DEFAULT_RETENTION_SECS)
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
    let cutoff = chrono::Utc::now().timestamp() - retention_seconds(conn, peer_id);
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
        chrono::Utc::now().timestamp()
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
