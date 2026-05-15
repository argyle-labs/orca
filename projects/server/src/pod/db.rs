//! Pod-mesh DB helpers. Schema lives in db::apply_schema (pod_discovery,
//! pod_pending_offers, pod_peers, pod_trust, pod_self).
//!
//! Code-hash storage: pairing codes are `sha256(raw_code)` only — the raw
//! 6-char code is shown to the user on both screens but never persisted in
//! plaintext on the joiner side.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// SHA-256 of a raw pairing code, hex-encoded (lowercase, 64 chars).
pub fn hash_code(raw: &str) -> String {
    let mut h = Sha256::new();
    h.update(raw.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in &d[..] {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
    }
    s
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── pod_discovery ────────────────────────────────────────────────────────────

/// One row per orca seen on the wire (mDNS or manual probe), keyed by
/// bootstrap pubkey fingerprint so IP/hostname churn doesn't fragment it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiscoveryRow {
    pub pubkey_fp: String,
    pub peer_id: Option<String>,
    pub hostname: String,
    pub addr: String,
    pub port: u16,
    pub state: String,
    pub can_invite: bool,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn upsert_discovery(
    conn: &Connection,
    pubkey_fp: &str,
    peer_id: Option<&str>,
    hostname: &str,
    addr: &str,
    port: u16,
    state: &str,
    can_invite: bool,
) -> Result<()> {
    let now = now_secs();
    // Evict stale rows for the same hostname carrying a different fp. A peer
    // that regenerates its bootstrap key (daemon reinstall, key rotation,
    // factory reset) advertises a new fp; the old row would otherwise live on
    // forever and the scheduler would keep dialing it and hitting
    // `pinned bootstrap pubkey mismatch`.
    conn.execute(
        "DELETE FROM pod_discovery WHERE hostname = ? AND pubkey_fp <> ?",
        params![hostname, pubkey_fp],
    )?;
    // Also drop any stale outbound offers pinned to the evicted fp so the
    // scheduler stops retrying them.
    conn.execute(
        "DELETE FROM pod_pending_offers
         WHERE direction = 'out' AND peer_hostname = ? AND peer_pubkey_fp <> ?",
        params![hostname, pubkey_fp],
    )?;
    conn.execute(
        "INSERT INTO pod_discovery
             (pubkey_fp, peer_id, hostname, addr, port, state, can_invite, first_seen_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(pubkey_fp) DO UPDATE SET
             peer_id      = COALESCE(excluded.peer_id, pod_discovery.peer_id),
             hostname     = excluded.hostname,
             addr         = excluded.addr,
             port         = excluded.port,
             state        = excluded.state,
             can_invite   = excluded.can_invite,
             last_seen_at = excluded.last_seen_at",
        params![
            pubkey_fp,
            peer_id,
            hostname,
            addr,
            port as i64,
            state,
            can_invite as i64,
            now,
            now
        ],
    )?;
    Ok(())
}

pub fn list_discovery(conn: &Connection) -> Result<Vec<DiscoveryRow>> {
    let mut stmt = conn.prepare(
        "SELECT pubkey_fp, peer_id, hostname, addr, port, state, can_invite,
                first_seen_at, last_seen_at
         FROM pod_discovery
         ORDER BY last_seen_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(DiscoveryRow {
            pubkey_fp: r.get(0)?,
            peer_id: r.get(1)?,
            hostname: r.get(2)?,
            addr: r.get(3)?,
            port: r.get::<_, i64>(4)? as u16,
            state: r.get(5)?,
            can_invite: r.get::<_, i64>(6)? != 0,
            first_seen_at: r.get(7)?,
            last_seen_at: r.get(8)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn list_unclaimed_discovery(conn: &Connection) -> Result<Vec<DiscoveryRow>> {
    Ok(list_discovery(conn)?
        .into_iter()
        .filter(|r| r.state == "unclaimed")
        .collect())
}

// ── pod_pending_offers ───────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingOffer {
    pub offer_id: String,
    pub direction: String, // "in" | "out"
    pub peer_pubkey_fp: String,
    pub peer_hostname: String,
    pub peer_addr: String,
    pub peer_port: u16,
    pub code_hash: String,
    pub mesh_ca_cert_pem: Option<String>,
    pub inviter_peer_id: Option<String>,
    pub pod_id: Option<String>,
    pub expires_at: i64,
    pub created_at: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_pending_offer(
    conn: &Connection,
    offer_id: &str,
    direction: &str,
    peer_pubkey_fp: &str,
    peer_hostname: &str,
    peer_addr: &str,
    peer_port: u16,
    code_hash: &str,
    mesh_ca_cert_pem: Option<&str>,
    inviter_peer_id: Option<&str>,
    pod_id: Option<&str>,
    ttl_secs: i64,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_pending_offers
             (offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
              code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            offer_id,
            direction,
            peer_pubkey_fp,
            peer_hostname,
            peer_addr,
            peer_port as i64,
            code_hash,
            mesh_ca_cert_pem,
            inviter_peer_id,
            pod_id,
            now + ttl_secs,
            now,
        ],
    )?;
    Ok(())
}

pub fn list_pending_offers(conn: &Connection, direction: &str) -> Result<Vec<PendingOffer>> {
    let now = now_secs();
    let mut stmt = conn.prepare(
        "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at
         FROM pod_pending_offers
         WHERE direction = ? AND expires_at >= ?
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![direction, now], |r| {
        Ok(PendingOffer {
            offer_id: r.get(0)?,
            direction: r.get(1)?,
            peer_pubkey_fp: r.get(2)?,
            peer_hostname: r.get(3)?,
            peer_addr: r.get(4)?,
            peer_port: r.get::<_, i64>(5)? as u16,
            code_hash: r.get(6)?,
            mesh_ca_cert_pem: r.get(7)?,
            inviter_peer_id: r.get(8)?,
            pod_id: r.get(9)?,
            expires_at: r.get(10)?,
            created_at: r.get(11)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Find an inbound pending offer by code (joiner side). Returns None if no
/// non-expired offer matches.
pub fn find_pending_offer_by_code(conn: &Connection, code: &str) -> Result<Option<PendingOffer>> {
    let code_hash = hash_code(code);
    let now = now_secs();
    let row = conn
        .query_row(
            "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                    code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at
             FROM pod_pending_offers
             WHERE direction = 'in' AND code_hash = ? AND expires_at >= ?",
            params![code_hash, now],
            |r| {
                Ok(PendingOffer {
                    offer_id: r.get(0)?,
                    direction: r.get(1)?,
                    peer_pubkey_fp: r.get(2)?,
                    peer_hostname: r.get(3)?,
                    peer_addr: r.get(4)?,
                    peer_port: r.get::<_, i64>(5)? as u16,
                    code_hash: r.get(6)?,
                    mesh_ca_cert_pem: r.get(7)?,
                    inviter_peer_id: r.get(8)?,
                    pod_id: r.get(9)?,
                    expires_at: r.get(10)?,
                    created_at: r.get(11)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Outbound side (inviter): verify code+pubkey match a pending outbound offer
/// and return the offer. Used by pod/join-confirm before signing CSRs.
pub fn find_outbound_offer_by_code_and_fp(
    conn: &Connection,
    code: &str,
    peer_pubkey_fp: &str,
) -> Result<Option<PendingOffer>> {
    let code_hash = hash_code(code);
    let now = now_secs();
    let row = conn
        .query_row(
            "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                    code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at
             FROM pod_pending_offers
             WHERE direction = 'out'
               AND code_hash = ?
               AND peer_pubkey_fp = ?
               AND expires_at >= ?",
            params![code_hash, peer_pubkey_fp, now],
            |r| {
                Ok(PendingOffer {
                    offer_id: r.get(0)?,
                    direction: r.get(1)?,
                    peer_pubkey_fp: r.get(2)?,
                    peer_hostname: r.get(3)?,
                    peer_addr: r.get(4)?,
                    peer_port: r.get::<_, i64>(5)? as u16,
                    code_hash: r.get(6)?,
                    mesh_ca_cert_pem: r.get(7)?,
                    inviter_peer_id: r.get(8)?,
                    pod_id: r.get(9)?,
                    expires_at: r.get(10)?,
                    created_at: r.get(11)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub fn delete_pending_offer(conn: &Connection, offer_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pod_pending_offers WHERE offer_id = ?",
        params![offer_id],
    )?;
    Ok(())
}

/// True if we already have an open outbound offer to this peer fp. Used by
/// the auto-offer scheduler to avoid spamming a target.
pub fn has_open_outbound_offer(conn: &Connection, peer_pubkey_fp: &str) -> Result<bool> {
    let now = now_secs();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pod_pending_offers
         WHERE direction = 'out' AND peer_pubkey_fp = ? AND expires_at >= ?",
        params![peer_pubkey_fp, now],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

// ── pod_peers ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerRow {
    pub peer_id: String,
    pub peer_hostname: String,
    pub peer_addr: String,
    pub peer_port: u16,
    pub pubkey_fp: Option<String>,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub departed_at: Option<i64>,
    pub local_secure: bool,
    pub peer_secure: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn upsert_peer(
    conn: &Connection,
    peer_id: &str,
    peer_hostname: &str,
    peer_addr: &str,
    peer_port: u16,
    pubkey_fp: Option<&str>,
    ca_cert_pem: &str,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_peers
             (peer_id, peer_hostname, peer_addr, peer_port, pubkey_fp, ca_cert_pem,
              first_seen_at, last_seen_at, departed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(peer_id) DO UPDATE SET
             peer_hostname = excluded.peer_hostname,
             peer_addr     = excluded.peer_addr,
             peer_port     = excluded.peer_port,
             pubkey_fp     = COALESCE(excluded.pubkey_fp, pod_peers.pubkey_fp),
             last_seen_at  = excluded.last_seen_at,
             departed_at   = NULL",
        params![
            peer_id,
            peer_hostname,
            peer_addr,
            peer_port as i64,
            pubkey_fp,
            ca_cert_pem,
            now,
            now
        ],
    )?;
    Ok(())
}

/// Self-heal upsert: ensure a `pod_peers` row exists for `peer_cn` so trust
/// inserts don't trip the FK on `pod_trust.peer_id`. Only inserts when no row
/// is present — existing rows are left untouched so an admin-set hostname or
/// pubkey_fp isn't overwritten by a notify dial.
///
/// Used by `handle_notify_trust` (and other CN-keyed handlers) to repair
/// legacy joiners that landed with `peer_id="unknown"` in rc.≤24 — the mTLS
/// CN is the trustworthy identifier, so we materialize a stub row keyed by
/// it on first contact.
pub fn ensure_peer_stub(
    conn: &Connection,
    peer_cn: &str,
    peer_addr: &str,
    peer_port: u16,
) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM pod_peers WHERE peer_id = ?",
            params![peer_cn],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_peers
             (peer_id, peer_hostname, peer_addr, peer_port, pubkey_fp, ca_cert_pem,
              first_seen_at, last_seen_at, departed_at)
         VALUES (?, ?, ?, ?, NULL, '', ?, ?, NULL)
         ON CONFLICT(peer_id) DO NOTHING",
        params![peer_cn, peer_cn, peer_addr, peer_port as i64, now, now],
    )?;
    Ok(())
}

pub fn list_peers(conn: &Connection) -> Result<Vec<PeerRow>> {
    let mut stmt = conn.prepare(
        "SELECT p.peer_id,
                COALESCE(d.hostname, p.peer_hostname) AS peer_hostname,
                p.peer_addr, p.peer_port, p.pubkey_fp,
                p.first_seen_at, p.last_seen_at, p.departed_at,
                COALESCE(t.local_secure, 0), COALESCE(t.peer_secure, 0)
         FROM pod_peers p
         LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
         LEFT JOIN pod_discovery d ON d.addr = p.peer_addr
         ORDER BY p.last_seen_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PeerRow {
            peer_id: r.get(0)?,
            peer_hostname: r.get(1)?,
            peer_addr: r.get(2)?,
            peer_port: r.get::<_, i64>(3)? as u16,
            pubkey_fp: r.get(4)?,
            first_seen_at: r.get(5)?,
            last_seen_at: r.get(6)?,
            departed_at: r.get(7)?,
            local_secure: r.get::<_, i64>(8)? != 0,
            peer_secure: r.get::<_, i64>(9)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Mark a peer as departed (received pod/peer-leaving). Trust bits go to 0
/// in the same transaction. Row is kept for audit; re-pairing clears departed_at.
pub fn mark_peer_departed(conn: &Connection, peer_id: &str) -> Result<()> {
    let now = now_secs();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE pod_peers SET departed_at = ?, last_seen_at = ? WHERE peer_id = ?",
        params![now, now, peer_id],
    )?;
    tx.execute(
        "UPDATE pod_trust SET local_secure = 0, peer_secure = 0, set_at = ? WHERE peer_id = ?",
        params![now, peer_id],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn is_peer_departed(conn: &Connection, peer_id: &str) -> Result<bool> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT departed_at FROM pod_peers WHERE peer_id = ?",
            params![peer_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    Ok(v.is_some())
}

/// Wipe all pod-membership state. Used by `pod leave`. Trust + peer rows are
/// dropped; pod_self is reset; the secrets table is NOT touched here (caller
/// decides via --wipe-secrets / --wipe-all flags).
pub fn wipe_pod_membership(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM pod_trust", [])?;
    tx.execute("DELETE FROM pod_peers", [])?;
    tx.execute("DELETE FROM pod_pending_offers", [])?;
    tx.execute("DELETE FROM pod_discovery", [])?;
    tx.execute(
        "INSERT INTO pod_self (id, self_secure, pod_id, set_at) VALUES (1, 0, NULL, ?)
         ON CONFLICT(id) DO UPDATE SET self_secure = 0, pod_id = NULL, set_at = excluded.set_at",
        params![now_secs()],
    )?;
    tx.commit()?;
    Ok(())
}

// ── pod_trust ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct TrustState {
    pub local_secure: bool,
    pub peer_secure: bool,
}

pub fn get_trust(conn: &Connection, peer_id: &str) -> Result<TrustState> {
    let row = conn
        .query_row(
            "SELECT local_secure, peer_secure FROM pod_trust WHERE peer_id = ?",
            params![peer_id],
            |r| {
                Ok(TrustState {
                    local_secure: r.get::<_, i64>(0)? != 0,
                    peer_secure: r.get::<_, i64>(1)? != 0,
                })
            },
        )
        .optional()?;
    Ok(row.unwrap_or(TrustState {
        local_secure: false,
        peer_secure: false,
    }))
}

pub fn set_trust(
    conn: &Connection,
    peer_id: &str,
    local_secure: Option<bool>,
    peer_secure: Option<bool>,
) -> Result<TrustState> {
    let prev = get_trust(conn, peer_id)?;
    let new = TrustState {
        local_secure: local_secure.unwrap_or(prev.local_secure),
        peer_secure: peer_secure.unwrap_or(prev.peer_secure),
    };
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_trust (peer_id, local_secure, peer_secure, set_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(peer_id) DO UPDATE SET
             local_secure = excluded.local_secure,
             peer_secure  = excluded.peer_secure,
             set_at       = excluded.set_at",
        params![
            peer_id,
            new.local_secure as i64,
            new.peer_secure as i64,
            now
        ],
    )?;
    Ok(new)
}

pub fn is_mutual_secure(t: TrustState) -> bool {
    t.local_secure && t.peer_secure
}

// ── pod_self ─────────────────────────────────────────────────────────────────

pub fn get_self_secure(conn: &Connection) -> Result<bool> {
    let row = conn
        .query_row("SELECT self_secure FROM pod_self WHERE id = 1", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?;
    Ok(row.unwrap_or(0) != 0)
}

pub fn set_self_secure(conn: &Connection, secure: bool) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_self (id, self_secure, set_at) VALUES (1, ?, ?)
         ON CONFLICT(id) DO UPDATE SET self_secure = excluded.self_secure, set_at = excluded.set_at",
        params![secure as i64, now],
    )?;
    Ok(())
}

pub fn get_pod_id(conn: &Connection) -> Result<Option<String>> {
    let row = conn
        .query_row("SELECT pod_id FROM pod_self WHERE id = 1", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?;
    Ok(row.flatten())
}

pub fn get_ca_previous_expires_at(conn: &Connection) -> Result<Option<i64>> {
    let row: Option<Option<i64>> = conn
        .query_row(
            "SELECT ca_previous_expires_at FROM pod_self WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.flatten())
}

pub fn set_ca_previous_expires_at(conn: &Connection, expires_at: Option<i64>) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_self (id, self_secure, ca_previous_expires_at, set_at)
         VALUES (1, 0, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             ca_previous_expires_at = excluded.ca_previous_expires_at,
             set_at = excluded.set_at",
        params![expires_at, now],
    )?;
    Ok(())
}

pub fn set_pod_id(conn: &Connection, pod_id: &str) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_self (id, self_secure, pod_id, set_at) VALUES (1, 0, ?, ?)
         ON CONFLICT(id) DO UPDATE SET pod_id = excluded.pod_id, set_at = excluded.set_at",
        params![pod_id, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_conn() -> (TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = db::open_unencrypted(&dir.path().join("orca.db")).expect("open_unencrypted");
        (dir, conn)
    }

    #[test]
    fn discovery_upsert_dedupes_by_fp() {
        let (_d, c) = test_conn();
        upsert_discovery(
            &c,
            "fp1",
            Some("peer.thor"),
            "thor",
            "10.0.0.5",
            12002,
            "unclaimed",
            false,
        )
        .unwrap();
        upsert_discovery(
            &c,
            "fp1",
            Some("peer.thor"),
            "thor",
            "10.0.0.6",
            12002,
            "pod:abc",
            true,
        )
        .unwrap();
        let rows = list_discovery(&c).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].addr, "10.0.0.6");
        assert_eq!(rows[0].state, "pod:abc");
        assert!(rows[0].can_invite);
    }

    #[test]
    fn pending_offer_roundtrip_and_lookup_by_code() {
        let (_d, c) = test_conn();
        let code = "4F2X9K";
        insert_pending_offer(
            &c,
            "off1",
            "in",
            "fpA",
            "mint",
            "10.0.0.1",
            12002,
            &hash_code(code),
            Some("CA-PEM"),
            Some("peer.mint"),
            Some("pod-1"),
            300,
        )
        .unwrap();
        let found = find_pending_offer_by_code(&c, code).unwrap().unwrap();
        assert_eq!(found.offer_id, "off1");
        assert_eq!(found.peer_hostname, "mint");
        assert!(find_pending_offer_by_code(&c, "BAD").unwrap().is_none());
    }

    #[test]
    fn expired_offer_not_returned() {
        let (_d, c) = test_conn();
        insert_pending_offer(
            &c,
            "off2",
            "in",
            "fpA",
            "mint",
            "10.0.0.1",
            12002,
            &hash_code("X"),
            None,
            None,
            None,
            -1,
        )
        .unwrap();
        assert!(find_pending_offer_by_code(&c, "X").unwrap().is_none());
    }

    #[test]
    fn peer_upsert_and_list() {
        let (_d, c) = test_conn();
        upsert_peer(
            &c,
            "peer.thor",
            "thor",
            "10.0.0.5",
            12002,
            Some("fp1"),
            "ca-pem",
        )
        .unwrap();
        let peers = list_peers(&c).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_addr, "10.0.0.5");
        assert_eq!(peers[0].peer_port, 12002);
        assert_eq!(peers[0].pubkey_fp.as_deref(), Some("fp1"));
        assert!(peers[0].departed_at.is_none());
    }

    #[test]
    fn peer_departed_resets_trust() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "peer.thor", "thor", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "peer.thor", Some(true), Some(true)).unwrap();
        mark_peer_departed(&c, "peer.thor").unwrap();
        assert!(is_peer_departed(&c, "peer.thor").unwrap());
        let t = get_trust(&c, "peer.thor").unwrap();
        assert!(!t.local_secure && !t.peer_secure);
    }

    #[test]
    fn rejoining_clears_departed() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "peer.thor", "thor", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        mark_peer_departed(&c, "peer.thor").unwrap();
        assert!(is_peer_departed(&c, "peer.thor").unwrap());
        upsert_peer(&c, "peer.thor", "thor", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        assert!(!is_peer_departed(&c, "peer.thor").unwrap());
    }

    #[test]
    fn trust_bits_independent() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "peer.thor", "thor", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "peer.thor", Some(true), None).unwrap();
        let t = get_trust(&c, "peer.thor").unwrap();
        assert!(t.local_secure && !t.peer_secure && !is_mutual_secure(t));
        set_trust(&c, "peer.thor", None, Some(true)).unwrap();
        assert!(is_mutual_secure(get_trust(&c, "peer.thor").unwrap()));
    }

    #[test]
    fn self_secure_and_pod_id() {
        let (_d, c) = test_conn();
        assert!(!get_self_secure(&c).unwrap());
        set_self_secure(&c, true).unwrap();
        assert!(get_self_secure(&c).unwrap());
        assert!(get_pod_id(&c).unwrap().is_none());
        set_pod_id(&c, "pod-xyz").unwrap();
        assert_eq!(get_pod_id(&c).unwrap().as_deref(), Some("pod-xyz"));
    }

    #[test]
    fn wipe_clears_state() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "peer.thor", "thor", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "peer.thor", Some(true), Some(true)).unwrap();
        upsert_discovery(
            &c,
            "fp1",
            None,
            "thor",
            "10.0.0.5",
            12002,
            "unclaimed",
            false,
        )
        .unwrap();
        set_self_secure(&c, true).unwrap();
        wipe_pod_membership(&c).unwrap();
        assert!(list_peers(&c).unwrap().is_empty());
        assert!(list_discovery(&c).unwrap().is_empty());
        assert!(!get_self_secure(&c).unwrap());
    }
}
