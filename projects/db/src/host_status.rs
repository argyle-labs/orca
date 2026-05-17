//! Per-peer system snapshot rows + per-peer cap enforcement.
//!
//! See `migrations/20260517170000__host_status.up.sql` for the schema.
//!
//! Authority model:
//!   * Rows with `source='local'` are owned by the host whose peer_id matches.
//!     The local persistence task writes these every ~60s.
//!   * Rows with `source='synced'` are mirrored from a peer's own DB by the
//!     pull-based sync task. They're read-only from this host's perspective.
//!
//! Cap: at most [`MAX_ROWS_PER_PEER`] rows per peer_id. Newer rows displace
//! older ones (FIFO). Enforced on every insert.

use anyhow::Result;
use rusqlite::{Connection, params};

/// Hard cap on snapshot history per peer. Keeps the table bounded regardless
/// of how aggressively a peer (or the local writer) churns out snapshots.
/// 1440 rows ≈ 24h at one snapshot per minute.
pub const MAX_ROWS_PER_PEER: usize = 1440;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostStatusRow {
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    pub payload_json: String,
    pub received_at_unix: i64,
    /// `"local"` (this host wrote it) or `"synced"` (mirrored from a peer).
    pub source: String,
}

/// Insert one snapshot and prune the per-peer history down to
/// [`MAX_ROWS_PER_PEER`]. Idempotent on `(peer_id, snapshot_at_unix)` — a
/// re-import of the same row is a no-op (INSERT OR IGNORE).
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
    // Prune. Sub-query gives the cutoff snapshot time; rows older than that
    // for this peer get deleted. Cheap because the index is on (peer_id, time DESC).
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
/// can't pull the entire 1440-row history if it doesn't need to.
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

    #[test]
    fn insert_and_latest_per_peer() {
        let conn = test_db();
        insert_status(&conn, "peer.a", 100, "{}", 100, "local").unwrap();
        insert_status(&conn, "peer.a", 200, "{}", 200, "local").unwrap();
        insert_status(&conn, "peer.b", 150, "{}", 150, "synced").unwrap();
        let rows = latest_per_peer(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        let a = rows.iter().find(|r| r.peer_id == "peer.a").unwrap();
        assert_eq!(a.snapshot_at_unix, 200);
    }

    #[test]
    fn insert_ignores_duplicate() {
        let conn = test_db();
        assert!(insert_status(&conn, "peer.a", 100, "{}", 100, "local").unwrap());
        assert!(!insert_status(&conn, "peer.a", 100, "{}", 200, "local").unwrap());
    }

    #[test]
    fn prune_caps_per_peer_history() {
        let conn = test_db();
        // Insert MAX + 5 rows, expect history trimmed to MAX.
        for i in 0..(MAX_ROWS_PER_PEER as i64 + 5) {
            insert_status(&conn, "peer.a", i, "{}", i, "local").unwrap();
        }
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM host_status WHERE peer_id='peer.a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n as usize, MAX_ROWS_PER_PEER);
        // Oldest 5 rows should be gone; latest preserved.
        let oldest: i64 = conn
            .query_row(
                "SELECT MIN(snapshot_at_unix) FROM host_status WHERE peer_id='peer.a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(oldest, 5);
    }

    #[test]
    fn rows_for_peer_respects_since() {
        let conn = test_db();
        insert_status(&conn, "peer.a", 100, "{}", 100, "local").unwrap();
        insert_status(&conn, "peer.a", 200, "{}", 200, "local").unwrap();
        insert_status(&conn, "peer.a", 300, "{}", 300, "local").unwrap();
        let rows = rows_for_peer(&conn, "peer.a", Some(150), 100).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].snapshot_at_unix, 300);
    }

    #[test]
    fn latest_snapshot_at_works() {
        let conn = test_db();
        assert_eq!(latest_snapshot_at(&conn, "peer.a").unwrap(), None);
        insert_status(&conn, "peer.a", 100, "{}", 100, "local").unwrap();
        insert_status(&conn, "peer.a", 300, "{}", 300, "local").unwrap();
        insert_status(&conn, "peer.a", 200, "{}", 200, "local").unwrap();
        assert_eq!(latest_snapshot_at(&conn, "peer.a").unwrap(), Some(300));
    }
}
