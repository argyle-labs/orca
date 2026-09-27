//! Read-only mesh_peers helpers exposed to the `fleet` domain crate's
//! mesh-related `#[orca_tool]`s.
//!
//! The mutating side of the mesh registry (offers, trust handshakes, wipes)
//! lives in `projects/db/src/mesh/peerdb.rs`, driven by the mTLS/bootstrap
//! state machine in `projects/system/src/mesh/`. This module exists so
//! callers can read the list of paired peers without pulling in that side.

use crate::host_addressing::{self, Routes};
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
    /// Multi-channel routes learned for this peer (LAN v4/v6, Tailscale,
    /// FQDN, etc.) as shared [`Route`]s. Empty if no rows in
    /// `mesh_peer_addresses` for this peer.
    #[serde(default)]
    pub routes: Routes,
    /// Bootstrap-pubkey fingerprint pinned for this peer, or `None` when the
    /// row was learned via roster-sync from a third-party peer (those rows
    /// arrive unpinned and need a transitive backfill).
    #[serde(default)]
    pub pubkey_fp: Option<String>,
}

/// Read the host-local `self_secure` flag from `mesh_self`. Returns `false`
/// when the row is absent (host hasn't opted into Tier-2 cred sync yet).
///
/// This is a read-only helper exposed to non-server crates that need to
/// surface the value in a snapshot. The mutating side (`set_self_secure`)
/// stays in `server::system::mesh::db` next to the Tier-2 state machine.
pub fn get_self_secure(conn: &Connection) -> Result<bool> {
    let row = conn
        .query_row("SELECT self_secure FROM mesh_self WHERE id = 1", [], |r| {
            r.get::<_, bool>(0)
        })
        .optional()?;
    Ok(row.unwrap_or(false))
}

pub fn list_peer_summaries(conn: &Connection) -> Result<Vec<PeerSummary>> {
    let mut stmt = conn.prepare(
        "SELECT p.peer_id, p.peer_hostname, p.peer_port,
                p.last_seen_at, p.departed_at,
                COALESCE(t.local_secure, 0), COALESCE(t.peer_secure, 0),
                p.pubkey_fp
         FROM mesh_peers p
         LEFT JOIN mesh_trust t ON t.peer_id = p.peer_id
         ORDER BY p.last_seen_at DESC",
    )?;
    let mut rows = stmt
        .query_map([], |r| {
            let departed_at: Option<i64> = r.get(4)?;
            let status = if departed_at.is_some() {
                "departed"
            } else {
                "active"
            }
            .to_string();
            Ok(PeerSummary {
                peer_id: r.get::<_, String>(0)?,
                hostname: r.get::<_, String>(1)?,
                // Derived from the peer's primary route below (peer_addr is no
                // longer a `mesh_peers` column).
                addr: String::new(),
                port: r.get::<_, i64>(2)? as u16,
                last_seen_at: r.get::<_, i64>(3)?,
                local_secure: r.get(5)?,
                peer_secure: r.get(6)?,
                status,
                routes: Routes::new(),
                pubkey_fp: r.get::<_, Option<String>>(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Attach per-peer routes, then derive the primary `addr` from them.
    // `mesh_peer_addresses` is the multi-route source of truth; each peer has
    // 0..N rows, so this stays an N+1 (peer counts are small).
    for peer in &mut rows {
        peer.routes = host_addressing::list_peer_addresses(conn, &peer.peer_id)?;
        peer.addr = peer
            .routes
            .primary()
            .map(|r| r.value.clone())
            .unwrap_or_default();
    }
    Ok(rows)
}

/// Refresh `mesh_peers.peer_hostname` for a peer when we learn its real OS
/// hostname (e.g. from a `mesh/ping` reply or a `host_status` snapshot).
/// No-op when `hostname` is empty so callers don't have to guard.
pub fn update_hostname(conn: &Connection, peer_id: &str, hostname: &str) -> Result<()> {
    if hostname.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE mesh_peers SET peer_hostname = ?1 WHERE peer_id = ?2 AND peer_hostname <> ?1",
        rusqlite::params![hostname, peer_id],
    )?;
    Ok(())
}
