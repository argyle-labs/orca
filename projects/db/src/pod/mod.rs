//! Read-only pod_peers helpers exposed to the `fleet` domain crate's
//! pod-related `#[orca_tool]`s.
//!
//! The mutating side of the pod registry (offers, trust handshakes, wipes)
//! lives in `projects/server/src/pod/db.rs` because it's wired into the
//! mTLS/bootstrap state machine. This module exists so non-server crates
//! can read the list of paired peers without taking a server dep.

use crate::host_addressing::{self, PodPeerAddress};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub mod peerdb;
pub use peerdb::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerSummary {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
    pub status: String,
    /// Multi-channel addresses learned for this peer (LAN v4/v6, Tailscale,
    /// FQDN, etc.). Empty if no rows in `pod_peer_addresses` for this peer.
    #[serde(default)]
    pub addresses: Vec<PodPeerAddress>,
    /// Bootstrap-pubkey fingerprint pinned for this peer, or `None` when the
    /// row was learned via roster-sync from a third-party peer (those rows
    /// arrive unpinned and need a transitive backfill).
    #[serde(default)]
    pub pubkey_fp: Option<String>,
}

/// Read the host-local `self_secure` flag from `pod_self`. Returns `false`
/// when the row is absent (host hasn't opted into Tier-2 cred sync yet).
///
/// This is a read-only helper exposed to non-server crates that need to
/// surface the value in a snapshot. The mutating side (`set_self_secure`)
/// stays in `server::pod::db` next to the Tier-2 state machine.
pub fn get_self_secure(conn: &Connection) -> Result<bool> {
    let row = conn
        .query_row("SELECT self_secure FROM pod_self WHERE id = 1", [], |r| {
            r.get::<_, bool>(0)
        })
        .optional()?;
    Ok(row.unwrap_or(false))
}

pub fn list_peer_summaries(conn: &Connection) -> Result<Vec<PeerSummary>> {
    let mut stmt = conn.prepare(
        "SELECT p.peer_id, p.peer_hostname, p.peer_addr, p.peer_port,
                p.last_seen_at, p.departed_at,
                COALESCE(t.local_secure, 0), COALESCE(t.peer_secure, 0),
                p.pubkey_fp
         FROM pod_peers p
         LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
         ORDER BY p.last_seen_at DESC",
    )?;
    let mut rows = stmt
        .query_map([], |r| {
            let departed_at: Option<i64> = r.get(5)?;
            let status = if departed_at.is_some() {
                "departed"
            } else {
                "active"
            }
            .to_string();
            Ok(PeerSummary {
                peer_id: r.get::<_, String>(0)?,
                hostname: r.get::<_, String>(1)?,
                addr: r.get::<_, String>(2)?,
                port: r.get::<_, i64>(3)? as u16,
                last_seen_at: r.get::<_, i64>(4)?,
                local_secure: r.get(6)?,
                peer_secure: r.get(7)?,
                status,
                addresses: Vec::new(),
                pubkey_fp: r.get::<_, Option<String>>(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Attach per-peer addresses. Done as a follow-up query (rather than a
    // JOIN in the main SELECT) because each peer has 0..N rows in
    // `pod_peer_addresses` — flattening would force a dedup pass on the
    // peer scalars. Peer counts are small (handful per pod), so N+1 is fine.
    for peer in &mut rows {
        peer.addresses = host_addressing::list_peer_addresses(conn, &peer.peer_id)?;
    }
    Ok(rows)
}

/// Refresh `pod_peers.peer_hostname` for a peer when we learn its real OS
/// hostname (e.g. from a `pod/ping` reply or a `host_status` snapshot).
/// No-op when `hostname` is empty so callers don't have to guard.
pub fn update_hostname(conn: &Connection, peer_id: &str, hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE pod_peers SET peer_hostname = ?1 WHERE peer_id = ?2 AND peer_hostname <> ?1",
        rusqlite::params![hostname, peer_id],
    )?;
    Ok(())
}
