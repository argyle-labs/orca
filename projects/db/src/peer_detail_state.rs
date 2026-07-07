//! Per-peer `system.detail {}` probe results.
//!
//! Written by the pod-side `peer_detail_probe` periodic task; read by
//! `pod.list` when building per-peer enrichment rows so the drawer can
//! hydrate from a fresh cached snapshot without an on-open RPC. Schema lives
//! in `migrations/20260602180000__peer_detail_state.up.sql`.
//!
//! The full `system.detail` payload is stored verbatim as JSON; consumers
//! deserialize the slice they need (currently the `system` field for the
//! SystemInfoReport). Failure semantics match `peer_update_state`: on
//! failure we leave the row alone; only successful probes advance
//! `checked_at`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone)]
pub struct PeerDetailState {
    pub peer_id: String,
    /// Raw JSON of the peer's `system.detail` result (a SystemStatusReport).
    pub payload: String,
    pub checked_at: i64,
}

pub fn upsert(conn: &Connection, row: &PeerDetailState) -> Result<()> {
    conn.execute(
        "INSERT INTO peer_detail_state (peer_id, payload, checked_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(peer_id) DO UPDATE SET
            payload    = excluded.payload,
            checked_at = excluded.checked_at",
        params![row.peer_id, row.payload, row.checked_at],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, peer_id: &str) -> Result<Option<PeerDetailState>> {
    let row = conn
        .query_row(
            "SELECT peer_id, payload, checked_at FROM peer_detail_state WHERE peer_id = ?1",
            params![peer_id],
            row_from,
        )
        .optional()?;
    Ok(row)
}

pub fn list_all(conn: &Connection) -> Result<Vec<PeerDetailState>> {
    let mut stmt = conn.prepare("SELECT peer_id, payload, checked_at FROM peer_detail_state")?;
    let rows = stmt
        .query_map([], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<PeerDetailState> {
    Ok(PeerDetailState {
        peer_id: r.get(0)?,
        payload: r.get(1)?,
        checked_at: r.get(2)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        crate::apply_schema(&c).unwrap();
        crate::run_pending_migrations(&c).unwrap();
        c
    }

    #[test]
    fn upsert_then_get_roundtrips() {
        let c = mem();
        let row = PeerDetailState {
            peer_id: "x".into(),
            payload: "{\"system\":{\"hostname\":\"x\"}}".into(),
            checked_at: 1_700_000_000,
        };
        upsert(&c, &row).unwrap();
        let got = get(&c, "x").unwrap().expect("present");
        assert_eq!(got.payload, "{\"system\":{\"hostname\":\"x\"}}");
        assert_eq!(got.checked_at, 1_700_000_000);
    }

    #[test]
    fn upsert_overwrites() {
        let c = mem();
        let mut row = PeerDetailState {
            peer_id: "x".into(),
            payload: "{}".into(),
            checked_at: 1,
        };
        upsert(&c, &row).unwrap();
        row.payload = "{\"a\":1}".into();
        row.checked_at = 2;
        upsert(&c, &row).unwrap();
        let got = get(&c, "x").unwrap().unwrap();
        assert_eq!(got.payload, "{\"a\":1}");
        assert_eq!(got.checked_at, 2);
    }
}
