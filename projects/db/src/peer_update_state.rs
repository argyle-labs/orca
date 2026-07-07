//! Per-peer `system.update {}` probe results.
//!
//! Written by the pod-side `peer_update_probe` periodic task; read by
//! `pod.list` when building per-peer enrichment rows. Schema lives in
//! `migrations/20260602120000__peer_update_state.up.sql`.
//!
//! Probe-failure semantics: on failure we leave the row alone, so the UI
//! keeps the last good values; only successful probes advance
//! `checked_at`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, Default)]
pub struct PeerUpdateState {
    pub peer_id: String,
    pub version: Option<String>,
    pub channel: Option<String>,
    pub pinned_to: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub checked_at: Option<i64>,
}

/// Upsert the latest successful probe. Caller has already filtered out
/// failures — this fn assumes the values are authoritative.
pub fn upsert(conn: &Connection, row: &PeerUpdateState) -> Result<()> {
    conn.execute(
        "INSERT INTO peer_update_state
            (peer_id, version, channel, pinned_to, latest, update_available, checked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(peer_id) DO UPDATE SET
            version          = excluded.version,
            channel          = excluded.channel,
            pinned_to        = excluded.pinned_to,
            latest           = excluded.latest,
            update_available = excluded.update_available,
            checked_at       = excluded.checked_at",
        params![
            row.peer_id,
            row.version,
            row.channel,
            row.pinned_to,
            row.latest,
            row.update_available as i64,
            row.checked_at,
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, peer_id: &str) -> Result<Option<PeerUpdateState>> {
    let row = conn
        .query_row(
            "SELECT peer_id, version, channel, pinned_to, latest, update_available, checked_at
             FROM peer_update_state WHERE peer_id = ?1",
            params![peer_id],
            row_from,
        )
        .optional()?;
    Ok(row)
}

pub fn list_all(conn: &Connection) -> Result<Vec<PeerUpdateState>> {
    let mut stmt = conn.prepare(
        "SELECT peer_id, version, channel, pinned_to, latest, update_available, checked_at
         FROM peer_update_state",
    )?;
    let rows = stmt
        .query_map([], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<PeerUpdateState> {
    Ok(PeerUpdateState {
        peer_id: r.get(0)?,
        version: r.get(1)?,
        channel: r.get(2)?,
        pinned_to: r.get(3)?,
        latest: r.get(4)?,
        update_available: r.get::<_, i64>(5)? != 0,
        checked_at: r.get(6)?,
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
        let row = PeerUpdateState {
            peer_id: "x".into(),
            version: Some("0.0.6-rc.8".into()),
            channel: Some("rc".into()),
            pinned_to: None,
            latest: Some("v0.0.6-rc.10".into()),
            update_available: true,
            checked_at: Some(1_700_000_000),
        };
        upsert(&c, &row).unwrap();
        let got = get(&c, "x").unwrap().expect("present");
        assert_eq!(got.version.as_deref(), Some("0.0.6-rc.8"));
        assert_eq!(got.channel.as_deref(), Some("rc"));
        assert!(got.update_available);
    }

    #[test]
    fn upsert_overwrites() {
        let c = mem();
        let mut row = PeerUpdateState {
            peer_id: "x".into(),
            version: Some("0.0.6-rc.8".into()),
            update_available: true,
            checked_at: Some(1),
            ..Default::default()
        };
        upsert(&c, &row).unwrap();
        row.version = Some("0.0.6-rc.10".into());
        row.update_available = false;
        row.checked_at = Some(2);
        upsert(&c, &row).unwrap();
        let got = get(&c, "x").unwrap().unwrap();
        assert_eq!(got.version.as_deref(), Some("0.0.6-rc.10"));
        assert!(!got.update_available);
        assert_eq!(got.checked_at, Some(2));
    }
}
