//! Host addressing — local host's own addressing snapshot + per-peer
//! address rows. See `project_host_addressing_plan.md` for the model.
//!
//! Two tables:
//!   * `host_addressing` (key, value, source, detected_at) — keyed by
//!     channel name (`display_name`, `fqdn`, `lan_v4`, `lan_v6`,
//!     `tailscale_v4`, `tailscale_v6`). PK = key, so a host gets one row
//!     per channel (multi-valued channels store the primary value;
//!     additional ones land in `pod_peer_addresses` on the peer side).
//!   * `pod_peer_addresses` (peer_id, kind, value, source, last_seen_at)
//!     — one peer → many rows. PK = (peer_id, kind, value).
//!
//! Source vocabulary: `manual` | `autodetect` | `caddy:<origin>`.

use anyhow::Result;
use rusqlite::{Connection, params};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostAddressingRow {
    pub key: String,
    pub value: String,
    pub source: String,
    pub detected_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PodPeerAddress {
    pub peer_id: String,
    pub kind: String,
    pub value: String,
    pub source: String,
    pub last_seen_at: i64,
}

use utils::time::now_secs_since_epoch as now_secs;

pub fn upsert_host_addressing(
    conn: &Connection,
    key: &str,
    value: &str,
    source: &str,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO host_addressing (key, value, source, detected_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key, value) DO UPDATE SET
             source      = excluded.source,
             detected_at = excluded.detected_at",
        params![key, value, source, now],
    )?;
    Ok(())
}

/// Replace **all** rows for a single-valued channel (`display_name`, `fqdn`)
/// with exactly one value. Multi-valued channels (`lan_v4`, `tailscale_v4`, …)
/// use [`upsert_host_addressing`] instead, which adds a row per distinct value.
pub fn set_host_addressing(conn: &Connection, key: &str, value: &str, source: &str) -> Result<()> {
    let now = now_secs();
    conn.execute("DELETE FROM host_addressing WHERE key = ?1", params![key])?;
    conn.execute(
        "INSERT INTO host_addressing (key, value, source, detected_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![key, value, source, now],
    )?;
    Ok(())
}

pub fn list_host_addressing(conn: &Connection) -> Result<Vec<HostAddressingRow>> {
    let mut stmt =
        conn.prepare("SELECT key, value, source, detected_at FROM host_addressing ORDER BY key")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(HostAddressingRow {
                key: r.get(0)?,
                value: r.get(1)?,
                source: r.get(2)?,
                detected_at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Delete all host_addressing rows whose `source` matches. Used by the
/// autodetect path to clear stale rows before re-inserting fresh ones.
pub fn clear_host_addressing_by_source(conn: &Connection, source: &str) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM host_addressing WHERE source = ?1",
        params![source],
    )?;
    Ok(n)
}

pub fn upsert_peer_address(
    conn: &Connection,
    peer_id: &str,
    kind: &str,
    value: &str,
    source: &str,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_peer_addresses (peer_id, kind, value, source, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(peer_id, kind, value) DO UPDATE SET
             source       = excluded.source,
             last_seen_at = excluded.last_seen_at",
        params![peer_id, kind, value, source, now],
    )?;
    Ok(())
}

/// Atomically replace the set of `pod_peer_addresses` rows for
/// `(peer_id, source)` with `entries` (`(kind, value)` pairs).
///
/// Used by the ping-driven refresh path: every successful `pod/ping` carries
/// the peer's full addressing snapshot from one source (`autodetect`), and we
/// want stale rows (addresses the peer no longer reports) to disappear. Other
/// sources (e.g. `manual`, `caddy:*`) are untouched.
pub fn replace_peer_addresses_from_source(
    conn: &mut Connection,
    peer_id: &str,
    source: &str,
    entries: &[(&str, &str)],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM pod_peer_addresses WHERE peer_id = ?1 AND source = ?2",
        params![peer_id, source],
    )?;
    let now = now_secs();
    for (kind, value) in entries {
        tx.execute(
            "INSERT INTO pod_peer_addresses (peer_id, kind, value, source, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(peer_id, kind, value) DO UPDATE SET
                 source       = excluded.source,
                 last_seen_at = excluded.last_seen_at",
            params![peer_id, kind, value, source, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn list_peer_addresses(conn: &Connection, peer_id: &str) -> Result<Vec<PodPeerAddress>> {
    let mut stmt = conn.prepare(
        "SELECT peer_id, kind, value, source, last_seen_at
         FROM pod_peer_addresses
         WHERE peer_id = ?1
         ORDER BY kind, value",
    )?;
    let rows = stmt
        .query_map(params![peer_id], |r| {
            Ok(PodPeerAddress {
                peer_id: r.get(0)?,
                kind: r.get(1)?,
                value: r.get(2)?,
                source: r.get(3)?,
                last_seen_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn host_addressing_roundtrip() {
        let conn = test_conn();
        set_host_addressing(&conn, "display_name", "host-i", "manual").unwrap();
        upsert_host_addressing(&conn, "lan_v4", "10.0.0.5", "autodetect").unwrap();
        let rows = list_host_addressing(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        // set_ replaces a single-valued channel in place.
        set_host_addressing(&conn, "display_name", "host-i-2", "manual").unwrap();
        let rows = list_host_addressing(&conn).unwrap();
        let dn: Vec<_> = rows.iter().filter(|r| r.key == "display_name").collect();
        assert_eq!(dn.len(), 1);
        assert_eq!(dn[0].value, "host-i-2");

        // A dual-homed host stores every LAN IPv4 as an equal row.
        upsert_host_addressing(&conn, "lan_v4", "10.0.0.6", "autodetect").unwrap();
        let rows = list_host_addressing(&conn).unwrap();
        let lan: Vec<_> = rows.iter().filter(|r| r.key == "lan_v4").collect();
        assert_eq!(lan.len(), 2);

        // Clear by source drops both autodetect lan_v4 rows.
        let n = clear_host_addressing_by_source(&conn, "autodetect").unwrap();
        assert_eq!(n, 2);
        let rows = list_host_addressing(&conn).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn peer_addresses_roundtrip() {
        let conn = test_conn();
        // pod_peer_addresses has FK to pod_peers; insert a peer row first.
        conn.execute(
            "INSERT INTO pod_peers (peer_id, peer_hostname, peer_addr, peer_port,
                                    ca_cert_pem, first_seen_at, last_seen_at)
             VALUES ('p1', 'host-g', '10.0.0.6', 9100, '', 0, 0)",
            [],
        )
        .unwrap();
        upsert_peer_address(&conn, "p1", "lan_v4", "10.0.0.6", "autodetect").unwrap();
        upsert_peer_address(&conn, "p1", "tailscale_v4", "100.64.1.2", "autodetect").unwrap();
        // Idempotent on same (peer,kind,value) — refresh source/last_seen_at.
        upsert_peer_address(&conn, "p1", "lan_v4", "10.0.0.6", "manual").unwrap();

        let rows = list_peer_addresses(&conn, "p1").unwrap();
        assert_eq!(rows.len(), 2);
        let lan = rows.iter().find(|r| r.kind == "lan_v4").unwrap();
        assert_eq!(lan.source, "manual");
    }

    #[test]
    fn replace_peer_addresses_replaces_only_matching_source() {
        let mut conn = test_conn();
        conn.execute(
            "INSERT INTO pod_peers (peer_id, peer_hostname, peer_addr, peer_port,
                                    ca_cert_pem, first_seen_at, last_seen_at)
             VALUES ('p1', 'host-g', '10.0.0.6', 9100, '', 0, 0)",
            [],
        )
        .unwrap();

        // Seed: 2 autodetect rows + 1 manual row.
        upsert_peer_address(&conn, "p1", "lan_v4", "10.0.0.6", "autodetect").unwrap();
        upsert_peer_address(&conn, "p1", "lan_v4", "10.0.0.7", "autodetect").unwrap();
        upsert_peer_address(&conn, "p1", "fqdn", "host-g.lan", "manual").unwrap();

        // Replace autodetect rows with a single fresh entry; manual row must survive.
        replace_peer_addresses_from_source(
            &mut conn,
            "p1",
            "autodetect",
            &[("lan_v4", "10.0.0.8"), ("tailscale_v4", "100.64.1.2")],
        )
        .unwrap();

        let rows = list_peer_addresses(&conn, "p1").unwrap();
        assert_eq!(rows.len(), 3);
        // Manual row preserved
        assert!(
            rows.iter()
                .any(|r| r.kind == "fqdn" && r.source == "manual")
        );
        // Old autodetect row gone (10.0.0.6 and 10.0.0.7 both removed)
        assert!(
            !rows
                .iter()
                .any(|r| r.source == "autodetect" && r.value == "10.0.0.6")
        );
        // New autodetect rows present
        assert!(rows.iter().any(|r| r.value == "10.0.0.8"));
        assert!(rows.iter().any(|r| r.value == "100.64.1.2"));
    }

    #[test]
    fn replace_peer_addresses_with_empty_entries_clears_source() {
        let mut conn = test_conn();
        conn.execute(
            "INSERT INTO pod_peers (peer_id, peer_hostname, peer_addr, peer_port,
                                    ca_cert_pem, first_seen_at, last_seen_at)
             VALUES ('p2', 'host-h', '10.0.0.9', 9100, '', 0, 0)",
            [],
        )
        .unwrap();
        upsert_peer_address(&conn, "p2", "lan_v4", "10.0.0.9", "autodetect").unwrap();
        replace_peer_addresses_from_source(&mut conn, "p2", "autodetect", &[]).unwrap();
        let rows = list_peer_addresses(&conn, "p2").unwrap();
        assert!(rows.is_empty());
    }
}
