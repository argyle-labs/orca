//! Pod-mesh DB helpers. Schema lives in db::apply_schema (pod_discovery,
//! pod_pending_offers, pod_peers, pod_trust, pod_self).
//!
//! Code-hash storage: pairing codes are `sha256(raw_code)` only — the raw
//! 6-char code is shown to the user on both screens but never persisted in
//! plaintext on the joiner side.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// SHA-256 of a raw pairing code, hex-encoded (lowercase, 64 chars).
pub fn hash_code(raw: &str) -> String {
    utils::hash::sha256_hex(raw.as_bytes())
}

use utils::time::now_secs_since_epoch as now_secs;

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

/// Delete pod_discovery rows whose hostname matches ours but whose pubkey_fp
/// differs — those are previous identities of THIS host (key rotation,
/// daemon reinstall, factory reset) that would otherwise show up as
/// "STALE SELF IDENTITY" in the UI on every deploy.
pub fn evict_stale_self(conn: &Connection, hostname: &str, pubkey_fp: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pod_discovery WHERE hostname = ? AND pubkey_fp <> ?",
        params![hostname, pubkey_fp],
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
    /// Plaintext pairing code — present on inbound offers when the inviter
    /// included it (mDNS-verified LAN peers). Allows auto-accept without
    /// out-of-band code entry.
    pub code_plain: Option<String>,
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
    code_plain: Option<&str>,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_pending_offers
             (offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
              code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
              code_plain)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            code_plain,
        ],
    )?;
    Ok(())
}

pub fn list_pending_offers(conn: &Connection, direction: &str) -> Result<Vec<PendingOffer>> {
    let now = now_secs();
    let mut stmt = conn.prepare(
        "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
                code_plain
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
            code_plain: r.get(12)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Find an inbound pending offer by code (joiner side). Returns None if no
/// non-expired offer matches.
/// Look up an inbound pending offer by raw pairing code, regardless of
/// expiry. Returns `None` only when the code doesn't match any offer at all,
/// so callers can distinguish "wrong code" from "expired offer" and surface
/// the right CLI message (per `project_pod_join_ux.md`: silent-on-failure is
/// the symptom we're fixing).
pub fn find_pending_offer_by_code_any_expiry(
    conn: &Connection,
    code: &str,
) -> Result<Option<PendingOffer>> {
    let code_hash = hash_code(code);
    let row = conn
        .query_row(
            "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                    code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
                    code_plain
             FROM pod_pending_offers
             WHERE direction = 'in' AND code_hash = ?",
            params![code_hash],
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
                    code_plain: r.get(12)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub fn find_pending_offer_by_code(conn: &Connection, code: &str) -> Result<Option<PendingOffer>> {
    let code_hash = hash_code(code);
    let now = now_secs();
    let row = conn
        .query_row(
            "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                    code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
                    code_plain
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
                    code_plain: r.get(12).unwrap_or(None),
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
                    code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
                    code_plain
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
                    code_plain: r.get(12).unwrap_or(None),
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

/// Delete every outbound pending offer pinned to `addr`, regardless of
/// expiry. Returns the number of rows removed. Used by the user-driven
/// re-invite path (idempotent +Add in the UI) and the explicit
/// `pod.cancel_offer` tool.
pub fn delete_outbound_offers_by_addr(conn: &Connection, addr: &str) -> Result<u32> {
    let n = conn.execute(
        "DELETE FROM pod_pending_offers WHERE direction = 'out' AND peer_addr = ?",
        params![addr],
    )?;
    Ok(n as u32)
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

/// Delete any legacy `pod_peers` row keyed by `"unknown"` that points at the
/// same `peer_addr` as a freshly-paired real peer. Pre-rc.25 mTLS clients
/// landed CN=`"unknown"` rows via `ensure_peer_stub`, and the
/// `host_status` puller still polls them forever even though they have no
/// usable identity. Call right after a successful pairing so the legacy row
/// doesn't linger as a parallel sibling next to the real one.
///
/// Best-effort: no error if nothing matched. Also cascades to `pod_trust`
/// via FK so we don't leave dangling trust rows.
pub fn cleanup_unknown_stub_at(conn: &Connection, peer_addr: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pod_trust WHERE peer_id = 'unknown' AND EXISTS (
             SELECT 1 FROM pod_peers WHERE peer_id = 'unknown' AND peer_addr = ?
         )",
        params![peer_addr],
    )?;
    conn.execute(
        "DELETE FROM pod_peers WHERE peer_id = 'unknown' AND peer_addr = ?",
        params![peer_addr],
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

/// The pinned bootstrap-pubkey fingerprint for a non-departed paired peer, if
/// recorded. Used by `pod/exec` authorization to bind a caller token's signer
/// to the peer authenticated on the mTLS wire. Returns `None` when the peer is
/// unknown, departed, or has no pinned fp (→ caller is unverifiable, refuse).
pub fn pinned_pubkey_fp(conn: &Connection, peer_id: &str) -> Result<Option<String>> {
    let fp = conn
        .query_row(
            "SELECT pubkey_fp FROM pod_peers WHERE peer_id = ? AND departed_at IS NULL",
            params![peer_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(fp)
}

/// True if a `pod_peers` row with this `peer_id` exists. Used by the
/// roster-sync loop to avoid double-counting newly-learned peers when an
/// upsert would otherwise be a silent no-op vs an actual insert.
pub fn peer_exists(conn: &Connection, peer_id: &str) -> Result<bool> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM pod_peers WHERE peer_id = ?",
            params![peer_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    Ok(exists)
}

/// Existence + raw `pubkey_fp` for a `pod_peers` row, regardless of
/// `departed_at`. Returns `None` when no row exists; `Some(None)` when the row
/// exists but has no pinned fp; `Some(Some(fp))` when pinned. Used by
/// roster-sync to distinguish "learn", "backfill", and "no-op" transitions
/// without spamming logs on every cycle.
pub fn peer_pubkey_fp_raw(conn: &Connection, peer_id: &str) -> Result<Option<Option<String>>> {
    let row = conn
        .query_row(
            "SELECT pubkey_fp FROM pod_peers WHERE peer_id = ?",
            params![peer_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row)
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

/// Clear a stale `departed_at` flag for a peer that's actually still reachable.
/// Used by `pod recover` after a misfired kick or a remote-driven false depart
/// (the 2026-05-28 kick/peer-leaving bug). Trust bits are NOT touched — the
/// operator must call `pod trust` separately if they want to re-establish
/// mutual trust.
pub fn unmark_peer_departed(conn: &Connection, peer_id: &str) -> Result<bool> {
    let now = now_secs();
    let updated = conn.execute(
        "UPDATE pod_peers SET departed_at = NULL, last_seen_at = ? WHERE peer_id = ? AND departed_at IS NOT NULL",
        params![now, peer_id],
    )?;
    Ok(updated > 0)
}

/// Hard-delete every local trace of a peer_id: pod_peers, pod_trust,
/// pod_discovery, and any outbound offers tied to it. Unlike
/// [`mark_peer_departed`] this leaves no audit row — it's the purge path for
/// `pod forget`, used to evict stale/orphan identities (machine_id churn,
/// decommissioned hosts) so they stop showing up in the roster. Returns the
/// total number of rows removed across all four tables.
pub fn forget_peer(conn: &Connection, peer_id: &str) -> Result<u32> {
    let tx = conn.unchecked_transaction()?;
    let mut removed = 0u32;
    removed += tx.execute("DELETE FROM pod_trust WHERE peer_id = ?", params![peer_id])? as u32;
    removed += tx.execute("DELETE FROM pod_peers WHERE peer_id = ?", params![peer_id])? as u32;
    removed += tx.execute(
        "DELETE FROM pod_discovery WHERE peer_id = ?",
        params![peer_id],
    )? as u32;
    removed += tx.execute(
        "DELETE FROM pod_pending_offers WHERE inviter_peer_id = ?",
        params![peer_id],
    )? as u32;
    tx.commit()?;
    Ok(removed)
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
            Some("host-g"),
            "host-g",
            "10.0.0.5",
            12002,
            "unclaimed",
            false,
        )
        .unwrap();
        upsert_discovery(
            &c,
            "fp1",
            Some("host-g"),
            "host-g",
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
            "host-i",
            "10.0.0.1",
            12002,
            &hash_code(code),
            Some("CA-PEM"),
            Some("host-i"),
            Some("pod-1"),
            300,
            None,
        )
        .unwrap();
        let found = find_pending_offer_by_code(&c, code).unwrap().unwrap();
        assert_eq!(found.offer_id, "off1");
        assert_eq!(found.peer_hostname, "host-i");
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
            "host-i",
            "10.0.0.1",
            12002,
            &hash_code("X"),
            None,
            None,
            None,
            -1,
            None,
        )
        .unwrap();
        assert!(find_pending_offer_by_code(&c, "X").unwrap().is_none());
    }

    #[test]
    fn peer_upsert_and_list() {
        let (_d, c) = test_conn();
        upsert_peer(
            &c,
            "host-g",
            "host-g",
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
        upsert_peer(&c, "host-g", "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "host-g", Some(true), Some(true)).unwrap();
        mark_peer_departed(&c, "host-g").unwrap();
        assert!(is_peer_departed(&c, "host-g").unwrap());
        let t = get_trust(&c, "host-g").unwrap();
        assert!(!t.local_secure && !t.peer_secure);
    }

    #[test]
    fn rejoining_clears_departed() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "host-g", "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        mark_peer_departed(&c, "host-g").unwrap();
        assert!(is_peer_departed(&c, "host-g").unwrap());
        upsert_peer(&c, "host-g", "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        assert!(!is_peer_departed(&c, "host-g").unwrap());
    }

    #[test]
    fn trust_bits_independent() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "host-g", "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "host-g", Some(true), None).unwrap();
        let t = get_trust(&c, "host-g").unwrap();
        assert!(t.local_secure && !t.peer_secure && !is_mutual_secure(t));
        set_trust(&c, "host-g", None, Some(true)).unwrap();
        assert!(is_mutual_secure(get_trust(&c, "host-g").unwrap()));
    }

    #[test]
    fn cleanup_unknown_stub_removes_matching_row_and_trust() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "unknown", "host-i", "10.0.0.1", 12002, None, "").unwrap();
        set_trust(&c, "unknown", Some(true), None).unwrap();
        upsert_peer(
            &c,
            "real",
            "host-i",
            "10.0.0.1",
            12002,
            Some("fp"),
            "ca-pem",
        )
        .unwrap();
        cleanup_unknown_stub_at(&c, "10.0.0.1").unwrap();
        let peers = list_peers(&c).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, "real");
        // Trust row for the stub must be gone too — no dangling FK ghost.
        let trust_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM pod_trust WHERE peer_id = 'unknown'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trust_count, 0);
    }

    #[test]
    fn cleanup_unknown_stub_at_different_addr_is_noop() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "unknown", "host-i", "10.0.0.1", 12002, None, "").unwrap();
        // Caller passes the addr of a NEW peer we just paired with — if that
        // addr doesn't match the stub, the stub stays (other host's leftover).
        cleanup_unknown_stub_at(&c, "10.0.0.2").unwrap();
        let peers = list_peers(&c).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, "unknown");
    }

    #[test]
    fn cleanup_unknown_stub_when_no_stub_present_is_noop() {
        let (_d, c) = test_conn();
        upsert_peer(
            &c,
            "real",
            "host-i",
            "10.0.0.1",
            12002,
            Some("fp"),
            "ca-pem",
        )
        .unwrap();
        cleanup_unknown_stub_at(&c, "10.0.0.1").unwrap();
        let peers = list_peers(&c).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, "real");
    }

    #[test]
    fn self_secure_and_pod_id() {
        let (_d, c) = test_conn();
        assert!(!db::pod::get_self_secure(&c).unwrap());
        set_self_secure(&c, true).unwrap();
        assert!(db::pod::get_self_secure(&c).unwrap());
        assert!(get_pod_id(&c).unwrap().is_none());
        set_pod_id(&c, "pod-xyz").unwrap();
        assert_eq!(get_pod_id(&c).unwrap().as_deref(), Some("pod-xyz"));
    }

    #[test]
    fn wipe_clears_state() {
        let (_d, c) = test_conn();
        upsert_peer(&c, "host-g", "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, "host-g", Some(true), Some(true)).unwrap();
        upsert_discovery(
            &c,
            "fp1",
            None,
            "host-g",
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
        assert!(!db::pod::get_self_secure(&c).unwrap());
    }
}
