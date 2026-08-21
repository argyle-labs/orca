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
    /// The inviter's self-advertised reachable addresses (LAN v4/v6,
    /// tailscale). The joiner tries each, pinned to `peer_pubkey_fp`, for
    /// join-confirm — robust to the TLS source IP being a tunnel address.
    /// Empty for offers from pre-candidate-addr inviters (falls back to
    /// `peer_addr`).
    pub candidate_addrs: Vec<String>,
}

/// Serialize/parse the CSV storage form of `candidate_addrs`.
fn join_addrs(addrs: &[String]) -> String {
    addrs.join(",")
}
fn split_addrs(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
    candidate_addrs: &[String],
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_pending_offers
             (offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
              code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
              code_plain, candidate_addrs)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
            join_addrs(candidate_addrs),
        ],
    )?;
    Ok(())
}

pub fn list_pending_offers(conn: &Connection, direction: &str) -> Result<Vec<PendingOffer>> {
    let now = now_secs();
    let mut stmt = conn.prepare(
        "SELECT offer_id, direction, peer_pubkey_fp, peer_hostname, peer_addr, peer_port,
                code_hash, mesh_ca_cert_pem, inviter_peer_id, pod_id, expires_at, created_at,
                code_plain, candidate_addrs
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
            candidate_addrs: split_addrs(&r.get::<_, String>(13)?),
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
                    code_plain, candidate_addrs
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
                    candidate_addrs: split_addrs(&r.get::<_, String>(13)?),
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
                    code_plain, candidate_addrs
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
                    candidate_addrs: r
                        .get::<_, String>(13)
                        .map(|s| split_addrs(&s))
                        .unwrap_or_default(),
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
                    code_plain, candidate_addrs
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
                    candidate_addrs: r
                        .get::<_, String>(13)
                        .map(|s| split_addrs(&s))
                        .unwrap_or_default(),
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

/// Correlated subquery yielding one `(peer_id, value)` row per peer: the
/// primary reachability route from `pod_peer_addresses`, chosen by channel
/// priority (lan_v4 > lan_v6 > tailscale_v4 > tailscale_v6 > other) then value.
/// This replaces the dropped `pod_peers.peer_addr` scalar — every reader that
/// used to select `p.peer_addr` LEFT JOINs this as `pa` and reads `pa.value`.
pub(crate) const PRIMARY_ROUTE: &str = "\
    SELECT peer_id, value FROM (\
        SELECT peer_id, value, ROW_NUMBER() OVER (\
            PARTITION BY peer_id ORDER BY \
            CASE kind \
                WHEN 'lan_v4' THEN 0 WHEN 'lan_v6' THEN 1 \
                WHEN 'tailscale_v4' THEN 2 WHEN 'tailscale_v6' THEN 3 ELSE 4 END, \
            value\
        ) AS rn FROM pod_peer_addresses\
    ) WHERE rn = 1";

#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerRow {
    pub peer_id: String,
    pub peer_hostname: String,
    /// Primary reachability address, DERIVED from the peer's `pod_peer_addresses`
    /// routes (see [`PRIMARY_ROUTE`]) — no longer a stored `pod_peers` column.
    /// Empty when the peer has no routes yet.
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
    // HARD RULE: every peer identity is a canonical uuidv7. Reject anything else
    // at the write boundary so no prefixed / truncated / legacy id is ever
    // persisted (the `peer.<id>`/`unclaimed.` machinery is retired).
    if !utils::id::is_uuidv7(peer_id) {
        anyhow::bail!(
            "refusing to persist peer identity {peer_id:?}: not a canonical uuidv7 (all ids must be uuidv7)"
        );
    }
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_peers
             (peer_id, peer_hostname, peer_port, pubkey_fp, ca_cert_pem,
              first_seen_at, last_seen_at, departed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
         ON CONFLICT(peer_id) DO UPDATE SET
             peer_hostname = excluded.peer_hostname,
             peer_port     = excluded.peer_port,
             pubkey_fp     = COALESCE(excluded.pubkey_fp, pod_peers.pubkey_fp),
             last_seen_at  = excluded.last_seen_at,
             departed_at   = NULL",
        params![
            peer_id,
            peer_hostname,
            peer_port as i64,
            pubkey_fp,
            ca_cert_pem,
            now,
            now
        ],
    )?;
    // Seed the primary address as a `pod_peer_addresses` route so a freshly
    // paired peer is dialable immediately (routes are the source of truth for
    // reachability now that `pod_peers.peer_addr` is gone). Ping later augments
    // it with the peer's full multi-channel set. No-op when empty or already
    // present (the (peer_id, kind, value) PK dedups).
    if !peer_addr.is_empty() {
        crate::host_addressing::upsert_peer_address(
            conn,
            peer_id,
            addr_route_kind(peer_addr),
            peer_addr,
            "bootstrap",
        )?;
    }
    Ok(())
}

/// Classify a bare address into a `pod_peer_addresses` route `kind`: `lan_v6`
/// when it carries a colon (an IPv6 literal), else `lan_v4`. An FQDN falls into
/// `lan_v4` — still dialable; ping refines the peer's channels afterward.
pub(crate) fn addr_route_kind(addr: &str) -> &'static str {
    if addr.contains(':') {
        "lan_v6"
    } else {
        "lan_v4"
    }
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
    // The "unknown" stub is matched by its reachability route (peer_addr is now
    // a `pod_peer_addresses` value, not a `pod_peers` column).
    let matches_addr =
        "EXISTS (SELECT 1 FROM pod_peer_addresses a WHERE a.peer_id = 'unknown' AND a.value = ?)";
    conn.execute(
        &format!("DELETE FROM pod_trust WHERE peer_id = 'unknown' AND {matches_addr}"),
        params![peer_addr],
    )?;
    conn.execute(
        &format!("DELETE FROM pod_peers WHERE peer_id = 'unknown' AND {matches_addr}"),
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
    // Full-uuid identity is a hard invariant. The CN is CA-validated, but a
    // legacy/pre-uuidv7 peer could still present a short or non-v7 CN; minting
    // a stub keyed by it would create a second-class identity row that never
    // converges onto the host's canonical uuidv7. Refuse rather than persist
    // it — the peer must re-present a canonical machine_id.
    if !utils::id::is_uuidv7(peer_cn) {
        anyhow::bail!(
            "refusing to stub peer identity {peer_cn:?}: not a canonical uuidv7 (full-uuid identity required)"
        );
    }
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
             (peer_id, peer_hostname, peer_port, pubkey_fp, ca_cert_pem,
              first_seen_at, last_seen_at, departed_at)
         VALUES (?, ?, ?, NULL, '', ?, ?, NULL)
         ON CONFLICT(peer_id) DO NOTHING",
        params![peer_cn, peer_cn, peer_port as i64, now, now],
    )?;
    // Seed the contact address as a route so the stub is dialable.
    if !peer_addr.is_empty() {
        crate::host_addressing::upsert_peer_address(
            conn,
            peer_cn,
            addr_route_kind(peer_addr),
            peer_addr,
            "bootstrap",
        )?;
    }
    Ok(())
}

/// Fold a set of stale sibling `pod_peers` rows into `canonical_id`, carrying
/// forward everything that must not be lost, then hard-delete the siblings
/// (their `pod_trust` + `pod_peer_addresses` cascade). Trust bits are OR'd in
/// (if any sibling trusted the peer, or was trusted, the canonical row
/// inherits it) and every address record is copied over — no reference or
/// controller path is dropped. `pubkey_fp` is deliberately NOT taken from a
/// sibling: a stale row may pin an old bootstrap key, and the authoritative fp
/// is refreshed against `canonical_id` by roster-sync / the live handshake.
///
/// Caller guarantees the canonical row already exists. No-op on an empty set.
fn merge_peer_rows(conn: &Connection, canonical_id: &str, stale_ids: &[String]) -> Result<()> {
    if stale_ids.is_empty() {
        return Ok(());
    }
    let now = now_secs();
    let tx = conn.unchecked_transaction()?;
    for sib in stale_ids {
        if sib == canonical_id {
            continue;
        }
        let (sl, sp): (i64, i64) = tx
            .query_row(
                "SELECT local_secure, peer_secure FROM pod_trust WHERE peer_id = ?",
                params![sib],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        if sl != 0 || sp != 0 {
            tx.execute(
                "INSERT INTO pod_trust (peer_id, local_secure, peer_secure, set_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(peer_id) DO UPDATE SET
                     local_secure = MAX(pod_trust.local_secure, excluded.local_secure),
                     peer_secure  = MAX(pod_trust.peer_secure, excluded.peer_secure),
                     set_at       = excluded.set_at",
                params![canonical_id, sl, sp, now],
            )?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO pod_peer_addresses (peer_id, kind, value, source, last_seen_at)
             SELECT ?1, kind, value, source, last_seen_at
             FROM pod_peer_addresses WHERE peer_id = ?2",
            params![canonical_id, sib],
        )?;
        tx.execute("DELETE FROM pod_peers WHERE peer_id = ?", params![sib])?;
    }
    tx.execute(
        "UPDATE pod_peers SET last_seen_at = ? WHERE peer_id = ?",
        params![now, canonical_id],
    )?;
    tx.commit()?;
    Ok(())
}

/// Self-healing identity convergence for one address. Given `canonical_id` —
/// the identity a host at `peer_addr` authoritatively presents right now (its
/// mTLS cert CN, already validated against the mesh CA by the caller) — fold
/// every OTHER non-departed `pod_peers` row at that address **that shares the
/// canonical row's pinned pubkey_fp** into it and retire the siblings. A
/// physical host accumulates parallel rows over its lifetime (e.g. a legacy
/// `peer.<id>` CN beside the bare id) that share one dial address AND one key;
/// this collapses them onto the one live id without losing a trust bit or
/// address record. The pubkey gate is essential: the CA-validated CN proves the
/// CONNECTING host's identity, not that a DIFFERENT host sharing this address is
/// the same machine — folding by address alone fuses distinct identities.
/// Returns the number of siblings retired. No-op when the address has a single
/// matching row, when `peer_addr` is empty, or when the canonical row does not
/// exist yet or has no pinned key.
pub fn reconcile_addr_to_canonical(
    conn: &Connection,
    canonical_id: &str,
    peer_addr: &str,
) -> Result<u32> {
    if peer_addr.is_empty() {
        return Ok(0);
    }
    let canon_exists: bool = conn
        .query_row(
            "SELECT 1 FROM pod_peers WHERE peer_id = ?",
            params![canonical_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !canon_exists {
        return Ok(0);
    }
    // Only fold siblings that share the canonical row's pinned key. The CN is
    // CA-authenticated, but that proves the identity of the CONNECTING host —
    // not that every OTHER row at this address is the same host. Two distinct
    // hosts can share a dial address (stale IPv6 / DHCP / NAT); folding by
    // address alone fuses them. If the canonical row has no pinned key yet,
    // skip address folding entirely rather than risk a wrong merge — the
    // handshake will pin the key and a later pass converges safely.
    let canon_fp: Option<String> = conn
        .query_row(
            "SELECT pubkey_fp FROM pod_peers WHERE peer_id = ?",
            params![canonical_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(canon_fp) = canon_fp else {
        return Ok(0);
    };
    let siblings: Vec<String> = {
        // Match by reachability route: a sibling shares the address when it has
        // a `pod_peer_addresses` row with this value (peer_addr is no longer a
        // `pod_peers` column).
        let mut stmt = conn.prepare(
            "SELECT p.peer_id FROM pod_peers p
             WHERE p.peer_id != ?2 AND p.departed_at IS NULL AND p.pubkey_fp = ?3
               AND EXISTS (SELECT 1 FROM pod_peer_addresses a
                           WHERE a.peer_id = p.peer_id AND a.value = ?1)",
        )?;
        let rows = stmt.query_map(params![peer_addr, canonical_id, canon_fp], |r| {
            r.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let n = siblings.len() as u32;
    merge_peer_rows(conn, canonical_id, &siblings)?;
    Ok(n)
}

/// Write-path identity convergence — the continuous, self-healing counterpart
/// to the boot ([`dedup_same_identity_rows`]) and handshake
/// ([`reconcile_addr_to_canonical`]) passes. Call it right after a peer row is
/// written (e.g. a roster-sync ingest): it folds every OTHER non-departed row
/// that belongs to the SAME physical host into one canonical row, matched by
/// EITHER the machine key (a legacy `peer.<id>` beside the bare id — same
/// machine, different id form) OR the dial address **paired with an identical
/// pinned pubkey_fp** (same key = same host). Address ALONE is never enough:
/// two distinct hosts can transiently share an address and must not be fused
/// (a re-keyed host — same address, new key — is intentionally left for the
/// authoritative re-pair, since address can't prove it's the same machine).
/// Canonical = most trust, then freshest, then lexically-stable id, so a secure
/// row is never folded into an insecure one and no trust bit or address record
/// is lost. Doing this on the write path stops
/// roster-sync from re-creating the split every cycle (which a periodic cleanup
/// can never win against). Returns the number of sibling rows retired.
pub fn converge_peer_identity(conn: &Connection, peer_id: &str, peer_addr: &str) -> Result<u32> {
    // The pinned key of the row we just wrote. Address-based folding is ONLY
    // safe when a sibling shares this key — two DISTINCT hosts can transiently
    // present the same dial address (stale IPv6 / DHCP reuse / NAT), and folding
    // them by address alone fuses them into one Frankenstein identity (one
    // host's uuid carrying another's hostname/addresses/key). Locality is a
    // flag, never an identity: same address ≠ same host, but same key does.
    let self_fp: Option<String> = conn
        .query_row(
            "SELECT pubkey_fp FROM pod_peers WHERE peer_id = ?",
            params![peer_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    struct R {
        id: String,
        last_seen: i64,
        trust: i64,
    }
    let mut cands: Vec<R> = Vec::new();
    {
        // `has_addr` = the peer carries `peer_addr` as one of its
        // `pod_peer_addresses` routes (peer_addr is no longer a `pod_peers`
        // column). Bound as ?1; an empty `peer_addr` matches nothing.
        let mut stmt = conn.prepare(
            "SELECT p.peer_id,
                    EXISTS(SELECT 1 FROM pod_peer_addresses a
                           WHERE a.peer_id = p.peer_id AND a.value = ?1) AS has_addr,
                    p.pubkey_fp, p.last_seen_at,
                    COALESCE(t.local_secure,0)+COALESCE(t.peer_secure,0)
             FROM pod_peers p
             LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
             WHERE p.departed_at IS NULL",
        )?;
        let rows = stmt.query_map(params![peer_addr], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (id, has_addr, fp, last_seen, trust) = row?;
            // Fold a sibling ONLY when it shares this address AND the identical
            // pinned key (both present and equal) — that is the same host under
            // a duplicate row. A different or missing key at the same address is
            // a different host (stale IPv6 / DHCP reuse / NAT); never fold it.
            // The self row matches here (its own address + key), so it is always
            // a candidate and becomes canonical when it wins the sort.
            let same_addr_same_key = !peer_addr.is_empty()
                && has_addr
                && matches!(
                    (self_fp.as_deref(), fp.as_deref()),
                    (Some(a), Some(b)) if a == b
                );
            if same_addr_same_key {
                cands.push(R {
                    id,
                    last_seen,
                    trust,
                });
            }
        }
    }
    if cands.len() < 2 {
        return Ok(0);
    }
    cands.sort_by(|a, b| {
        b.trust
            .cmp(&a.trust)
            .then(b.last_seen.cmp(&a.last_seen))
            .then(a.id.cmp(&b.id))
    });
    let canonical = cands[0].id.clone();
    let stale: Vec<String> = cands[1..].iter().map(|r| r.id.clone()).collect();
    let n = stale.len() as u32;
    merge_peer_rows(conn, &canonical, &stale)?;
    Ok(n)
}

/// Boot / upgrade reconcile pass: collapse `pod_peers` rows that are provably
/// the SAME identity — same dial address AND same pinned bootstrap `pubkey_fp`
/// — into one canonical row. This is the automatic cleanup that runs when a
/// host restarts onto a new build (the rollout migration path): it clears the
/// unambiguous duplicates (e.g. a legacy `peer.<id>` row beside the bare id,
/// both pinned to the same key) without needing a live handshake. Rows for the
/// same host under a DIFFERENT key (a re-keyed identity) are intentionally left
/// for the authoritative handshake path ([`reconcile_addr_to_canonical`]) to
/// converge, since only the live cert CN can say which key is current. Returns
/// the number of rows retired.
pub fn dedup_same_identity_rows(conn: &Connection) -> Result<u32> {
    struct R {
        id: String,
        last_seen: i64,
        trust: i64,
    }
    let mut groups: std::collections::BTreeMap<(String, String), Vec<R>> = Default::default();
    {
        // Group by (primary route, pinned key). The primary address is derived
        // from `pod_peer_addresses` (see PRIMARY_ROUTE) — peers with no route
        // are excluded (INNER-equivalent via the `pa.value != ''` filter).
        let mut stmt = conn.prepare(&format!(
            "SELECT p.peer_id, pa.value, p.pubkey_fp, p.last_seen_at,
                    COALESCE(t.local_secure,0), COALESCE(t.peer_secure,0)
             FROM pod_peers p
             LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
             LEFT JOIN ({PRIMARY_ROUTE}) pa ON pa.peer_id = p.peer_id
             WHERE p.departed_at IS NULL AND p.pubkey_fp IS NOT NULL
               AND pa.value IS NOT NULL AND pa.value != ''"
        ))?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let addr: String = r.get(1)?;
            let fp: String = r.get(2)?;
            let last_seen: i64 = r.get(3)?;
            let ls: i64 = r.get(4)?;
            let ps: i64 = r.get(5)?;
            Ok(((addr, fp), id, last_seen, ls + ps))
        })?;
        for row in rows {
            let (key, id, last_seen, trust) = row?;
            groups.entry(key).or_default().push(R {
                id,
                last_seen,
                trust,
            });
        }
    }

    let mut retired = 0u32;
    for (_key, mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        // Canonical = most trust, then freshest, then lexically-stable id.
        group.sort_by(|a, b| {
            b.trust
                .cmp(&a.trust)
                .then(b.last_seen.cmp(&a.last_seen))
                .then(a.id.cmp(&b.id))
        });
        let canonical = group[0].id.clone();
        let stale: Vec<String> = group[1..].iter().map(|r| r.id.clone()).collect();
        retired += stale.len() as u32;
        merge_peer_rows(conn, &canonical, &stale)?;
    }
    Ok(retired)
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
    let mut stmt = conn.prepare(&format!(
        "SELECT p.peer_id,
                COALESCE(d.hostname, p.peer_hostname) AS peer_hostname,
                COALESCE(pa.value, '') AS peer_addr, p.peer_port, p.pubkey_fp,
                p.first_seen_at, p.last_seen_at, p.departed_at,
                COALESCE(t.local_secure, 0), COALESCE(t.peer_secure, 0)
         FROM pod_peers p
         LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
         LEFT JOIN ({PRIMARY_ROUTE}) pa ON pa.peer_id = p.peer_id
         LEFT JOIN pod_discovery d ON d.addr = pa.value
         ORDER BY p.last_seen_at DESC"
    ))?;
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
    // Durable, replicated forget-tombstone. Without this a hard DELETE is silent:
    // an OFFLINE straggler that missed the `pod/peer-forget` fan-out still holds a
    // live peer row and re-gossips it on the next roster tick, resurrecting the
    // forgotten peer fleet-wide (issue #232). The tombstone rides the same
    // command-log transport as config deletes (see replication_ops), so a
    // reconnecting straggler learns of the forget and suppresses the resurrection.
    // A peer_id is uuidv7 and never reused — a genuine rejoin uses a NEW identity
    // (see retire_superseded_identities) — so tombstoning the old id is correct.
    write_forget_tombstone(&tx, peer_id)?;
    tx.commit()?;
    Ok(removed)
}

/// Replicated entity/key-column under which a forgotten `peer_id` is tombstoned
/// in the command-log (see [`crate::replication_ops`]). The entity is the real
/// `pod_peers` table so a merged tombstone's `apply_pending_deletes` also
/// physically evicts a resurrected row on any peer.
const PEER_TOMBSTONE_ENTITY: &str = "pod_peers";
const PEER_TOMBSTONE_KEY_COL: &str = "peer_id";

/// Write the durable forget-tombstone for `peer_id`. Idempotent: LWW keyed by
/// `(entity, key_val)` in [`crate::replication_ops::note_delete`].
pub fn write_forget_tombstone(conn: &Connection, peer_id: &str) -> Result<()> {
    crate::replication_ops::note_delete(
        conn,
        PEER_TOMBSTONE_ENTITY,
        PEER_TOMBSTONE_KEY_COL,
        peer_id,
        utils::time::now_millis_since_epoch(),
    )
}

/// True iff `peer_id` carries an *active* forget-tombstone: it was forgotten
/// within the TTL window and must NOT be re-ingested from a peer's roster. The
/// TTL bounds suppression so a legitimately re-pairing host (which uses a NEW
/// uuidv7 identity anyway) is never blocked forever, and matches the command-log
/// reap horizon so the tombstone and its suppression expire together.
pub fn is_peer_forgotten(conn: &Connection, peer_id: &str) -> Result<bool> {
    crate::replication_ops::is_deleted(
        conn,
        PEER_TOMBSTONE_ENTITY,
        peer_id,
        utils::time::now_millis_since_epoch(),
        crate::replication_ops::DEFAULT_TTL_MS,
    )
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

    // Canonical uuidv7 test identities (all ids MUST be uuidv7 — the legacy
    // `peer.`/`unclaimed.` prefix forms are retired). Distinct hosts get
    // distinct ids; a "duplicate row for the same host" is modeled as a second
    // uuidv7 sharing the same address + pinned key.
    const FREYR: &str = "019e7105-0000-7000-8000-0000000f0001";
    const FREYR_DUP: &str = "019e7105-0000-7000-8000-0000000f0002";
    const MAPLE: &str = "019e7105-0000-7000-8000-0000000a0001";
    const MAPLE_DUP: &str = "019e7105-0000-7000-8000-0000000a0002";
    const HOSTG: &str = "019e7105-0000-7000-8000-000000900001";
    const HOSTX: &str = "019e7105-0000-7000-8000-000000000011";
    const HOSTY: &str = "019e7105-0000-7000-8000-000000000012";
    const REAL: &str = "019e7105-0000-7000-8000-000000000021";

    /// Insert a legacy `"unknown"` stub row (a non-uuidv7 placeholder from
    /// rc.≤24) directly, bypassing the uuidv7 guard on `upsert_peer` — these
    /// rows can only pre-exist, never be freshly minted, and this exercises the
    /// cleanup path that retires them. Seeds a matching route so the
    /// address-based cleanup can find it.
    fn insert_legacy_unknown_stub(c: &Connection, addr: &str) {
        c.execute(
            "INSERT INTO pod_peers (peer_id, peer_hostname, peer_port, ca_cert_pem,
                                    first_seen_at, last_seen_at)
             VALUES ('unknown', 'host-i', 12002, '', 0, 0)",
            [],
        )
        .unwrap();
        crate::host_addressing::upsert_peer_address(c, "unknown", "lan_v4", addr, "bootstrap")
            .unwrap();
    }

    #[test]
    fn ensure_peer_stub_rejects_non_uuidv7_cn() {
        let (_d, c) = test_conn();
        // A pre-uuidv7 / short / prefixed CN must never mint an identity row —
        // full-uuid identity is a hard invariant.
        for bad in [
            "019e7105-991",
            "c56ccc7c2039",
            "peer.019e7105-991b",
            "unknown",
        ] {
            let err = ensure_peer_stub(&c, bad, "10.0.0.9", 12002).unwrap_err();
            assert!(
                err.to_string().contains("uuidv7"),
                "expected uuidv7 refusal for {bad:?}, got: {err}"
            );
        }
        assert!(active_ids(&c).is_empty(), "no stub rows may be created");

        // A canonical uuidv7 CN is accepted.
        let good = utils::id::new();
        ensure_peer_stub(&c, &good, "10.0.0.9", 12002).unwrap();
        assert!(active_ids(&c).contains(&good));
    }

    #[test]
    fn forget_writes_durable_replicated_tombstone() {
        let (_d, c) = test_conn();
        forget_peer(&c, MAPLE).unwrap();
        // A `delete` op for (pod_peers, peer_id) now exists in the command-log so
        // the eviction replicates and cannot be resurrected.
        let op: String = c
            .query_row(
                "SELECT op FROM replication_ops WHERE entity = 'pod_peers' AND key_val = ?1",
                params![MAPLE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "delete");
        // The ingest gate reports the peer as forgotten.
        assert!(is_peer_forgotten(&c, MAPLE).unwrap());
        // An unrelated peer is unaffected.
        assert!(!is_peer_forgotten(&c, FREYR).unwrap());
    }

    #[test]
    fn forget_tombstone_expires_after_ttl() {
        let (_d, c) = test_conn();
        write_forget_tombstone(&c, MAPLE).unwrap();
        // Within the TTL window the peer is suppressed.
        let now = utils::time::now_millis_since_epoch();
        assert!(
            crate::replication_ops::is_deleted(
                &c,
                "pod_peers",
                MAPLE,
                now,
                crate::replication_ops::DEFAULT_TTL_MS
            )
            .unwrap()
        );
        // Once `now` moves past stamp + TTL, suppression lifts so a peer that
        // legitimately re-pairs (with a new identity) is not blocked forever.
        let future = now + crate::replication_ops::DEFAULT_TTL_MS + 1;
        assert!(
            !crate::replication_ops::is_deleted(
                &c,
                "pod_peers",
                MAPLE,
                future,
                crate::replication_ops::DEFAULT_TTL_MS
            )
            .unwrap()
        );
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

    fn active_ids(c: &Connection) -> std::collections::HashSet<String> {
        list_peers(c)
            .unwrap()
            .into_iter()
            .filter(|r| r.departed_at.is_none())
            .map(|r| r.peer_id)
            .collect()
    }

    #[test]
    fn converge_never_fuses_distinct_hosts_sharing_an_address() {
        let (_d, c) = test_conn();
        // Two DIFFERENT hosts (distinct keys) transiently at the same dial
        // address — the baldur/willow scramble. Convergence must NOT fold them.
        let baldur = "019e7105-5f48-7b22-beba-525ada45ac37";
        let willow = "019f9cab-a9c9-7352-a9e8-2d05d0545340";
        upsert_peer(
            &c,
            baldur,
            "baldur",
            "10.10.10.6",
            12002,
            Some("fp-baldur"),
            "ca",
        )
        .unwrap();
        upsert_peer(
            &c,
            willow,
            "willow",
            "10.10.10.6",
            12002,
            Some("fp-willow"),
            "ca",
        )
        .unwrap();
        assert_eq!(
            converge_peer_identity(&c, willow, "10.10.10.6").unwrap(),
            0,
            "distinct keys at the same address must never fuse"
        );
        let ids = active_ids(&c);
        assert!(
            ids.contains(baldur) && ids.contains(willow),
            "both distinct identities survive"
        );

        // Same address AND same key = genuinely the same host → DOES fold.
        let dup = "019e7105-aaaa-7bbb-8ccc-000000000001";
        upsert_peer(
            &c,
            dup,
            "baldur",
            "10.10.10.6",
            12002,
            Some("fp-baldur"),
            "ca",
        )
        .unwrap();
        assert!(
            converge_peer_identity(&c, dup, "10.10.10.6").unwrap() >= 1,
            "same address + same key folds"
        );
    }

    #[test]
    fn reconcile_does_not_fold_different_key_at_same_address() {
        let (_d, c) = test_conn();
        // CA-authenticated canonical + a DIFFERENT host that merely shares the
        // address (different key) — must be left alone, not fused.
        upsert_peer(
            &c,
            "019e7105-5f48-7b22-beba-525ada45ac37",
            "baldur",
            "10.10.10.6",
            12002,
            Some("fp-baldur"),
            "ca",
        )
        .unwrap();
        upsert_peer(
            &c,
            "019f9cab-a9c9-7352-a9e8-2d05d0545340",
            "willow",
            "10.10.10.6",
            12002,
            Some("fp-willow"),
            "ca",
        )
        .unwrap();
        assert_eq!(
            reconcile_addr_to_canonical(&c, "019e7105-5f48-7b22-beba-525ada45ac37", "10.10.10.6")
                .unwrap(),
            0,
            "different key at same address is a different host"
        );
        assert_eq!(active_ids(&c).len(), 2);
    }

    #[test]
    fn reconcile_addr_folds_siblings_into_canonical() {
        let (_d, c) = test_conn();
        // Duplicate rows for one host: two uuidv7 ids sharing one address AND
        // one pinned key. The canonical row + a stale sibling that carries trust
        // and an address record that must survive the fold.
        upsert_peer(&c, FREYR, "freyr", "192.0.2.15", 12002, Some("fp"), "ca").unwrap();
        upsert_peer(
            &c,
            FREYR_DUP,
            "freyr",
            "192.0.2.15",
            12002,
            Some("fp"),
            "ca",
        )
        .unwrap();
        set_trust(&c, FREYR_DUP, Some(true), Some(true)).unwrap();

        let n = reconcile_addr_to_canonical(&c, FREYR, "192.0.2.15").unwrap();
        assert_eq!(n, 1);
        let ids = active_ids(&c);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(FREYR), "canonical row kept");
        let t = get_trust(&c, FREYR).unwrap();
        assert!(t.local_secure && t.peer_secure, "trust OR'd onto canonical");
        let addr_cnt: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM pod_peer_addresses WHERE peer_id=?1",
                params![FREYR],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(addr_cnt, 1, "sibling address folded forward");
    }

    #[test]
    fn reconcile_leaves_distinct_addresses_alone() {
        let (_d, c) = test_conn();
        upsert_peer(&c, HOSTX, "hostx", "10.0.0.1", 12002, Some("fp"), "ca").unwrap();
        upsert_peer(&c, HOSTY, "hostx", "10.0.0.2", 12002, Some("fp"), "ca").unwrap();
        // Distinct addresses = distinct hosts: never collapse.
        assert_eq!(
            reconcile_addr_to_canonical(&c, HOSTX, "10.0.0.1").unwrap(),
            0
        );
        assert_eq!(active_ids(&c).len(), 2);
    }

    #[test]
    fn reconcile_noop_when_canonical_row_absent() {
        let (_d, c) = test_conn();
        upsert_peer(&c, HOSTX, "h", "10.0.0.9", 12002, Some("fp"), "ca").unwrap();
        // Canonical id has no row yet — don't retire the only row we have.
        assert_eq!(
            reconcile_addr_to_canonical(&c, HOSTY, "10.0.0.9").unwrap(),
            0
        );
        assert_eq!(active_ids(&c).len(), 1);
    }

    #[test]
    fn dedup_same_identity_collapses_only_same_addr_and_fp() {
        let (_d, c) = test_conn();
        // freyr: two rows, SAME addr + SAME fp → collapse (keep the trusted one).
        upsert_peer(&c, FREYR, "freyr", "192.0.2.15", 12002, Some("fpF"), "ca").unwrap();
        upsert_peer(
            &c,
            FREYR_DUP,
            "freyr",
            "192.0.2.15",
            12002,
            Some("fpF"),
            "ca",
        )
        .unwrap();
        set_trust(&c, FREYR_DUP, Some(true), None).unwrap();
        // maple: two rows, SAME addr but DIFFERENT fp (re-keyed) → left for the
        // handshake path.
        upsert_peer(&c, MAPLE, "maple", "192.0.2.11", 12002, Some("fpA"), "ca").unwrap();
        upsert_peer(
            &c,
            MAPLE_DUP,
            "maple",
            "192.0.2.11",
            12002,
            Some("fpB"),
            "ca",
        )
        .unwrap();

        let retired = dedup_same_identity_rows(&c).unwrap();
        assert_eq!(
            retired, 1,
            "only the same-addr same-fp freyr pair collapses"
        );
        let ids = active_ids(&c);
        assert!(ids.contains(FREYR_DUP), "freyr canonical = trusted row");
        assert!(!ids.contains(FREYR), "freyr untrusted dup retired");
        assert!(
            ids.contains(MAPLE) && ids.contains(MAPLE_DUP),
            "maple re-keyed rows left for handshake convergence"
        );
    }

    #[test]
    fn converge_folds_duplicate_row_onto_secure_row() {
        let (_d, c) = test_conn();
        // Two uuidv7 rows for one host (same addr + same key): one insecure, one
        // secure. Convergence must fold into the SECURE row, never the reverse.
        upsert_peer(&c, FREYR, "freyr", "192.0.2.15", 12002, Some("fp"), "ca").unwrap();
        upsert_peer(
            &c,
            FREYR_DUP,
            "freyr",
            "192.0.2.15",
            12002,
            Some("fp"),
            "ca",
        )
        .unwrap();
        set_trust(&c, FREYR_DUP, Some(true), Some(true)).unwrap();

        let n = converge_peer_identity(&c, FREYR, "192.0.2.15").unwrap();
        assert_eq!(n, 1);
        let ids = active_ids(&c);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(FREYR_DUP), "secure row is canonical");
    }

    #[test]
    fn converge_leaves_distinct_hosts_alone() {
        let (_d, c) = test_conn();
        upsert_peer(&c, HOSTX, "hostx", "10.0.0.1", 12002, Some("f1"), "ca").unwrap();
        upsert_peer(&c, HOSTY, "hosty", "10.0.0.2", 12002, Some("f2"), "ca").unwrap();
        // Different key AND different addr = different hosts: no merge.
        assert_eq!(converge_peer_identity(&c, HOSTX, "10.0.0.1").unwrap(), 0);
        assert_eq!(active_ids(&c).len(), 2);
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
            &["10.0.0.1".to_string(), "100.64.0.1".to_string()],
        )
        .unwrap();
        let found = find_pending_offer_by_code(&c, code).unwrap().unwrap();
        assert_eq!(found.offer_id, "off1");
        assert_eq!(found.peer_hostname, "host-i");
        // candidate_addrs round-trips through the CSV storage form.
        assert_eq!(found.candidate_addrs, vec!["10.0.0.1", "100.64.0.1"]);
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
            &[],
        )
        .unwrap();
        assert!(find_pending_offer_by_code(&c, "X").unwrap().is_none());
    }

    #[test]
    fn peer_upsert_and_list() {
        let (_d, c) = test_conn();
        upsert_peer(
            &c,
            HOSTG,
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
        upsert_peer(&c, HOSTG, "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, HOSTG, Some(true), Some(true)).unwrap();
        mark_peer_departed(&c, HOSTG).unwrap();
        assert!(is_peer_departed(&c, HOSTG).unwrap());
        let t = get_trust(&c, HOSTG).unwrap();
        assert!(!t.local_secure && !t.peer_secure);
    }

    #[test]
    fn rejoining_clears_departed() {
        let (_d, c) = test_conn();
        upsert_peer(&c, HOSTG, "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        mark_peer_departed(&c, HOSTG).unwrap();
        assert!(is_peer_departed(&c, HOSTG).unwrap());
        upsert_peer(&c, HOSTG, "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        assert!(!is_peer_departed(&c, HOSTG).unwrap());
    }

    #[test]
    fn trust_bits_independent() {
        let (_d, c) = test_conn();
        upsert_peer(&c, HOSTG, "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, HOSTG, Some(true), None).unwrap();
        let t = get_trust(&c, HOSTG).unwrap();
        assert!(t.local_secure && !t.peer_secure && !is_mutual_secure(t));
        set_trust(&c, HOSTG, None, Some(true)).unwrap();
        assert!(is_mutual_secure(get_trust(&c, HOSTG).unwrap()));
    }

    #[test]
    fn cleanup_unknown_stub_removes_matching_row_and_trust() {
        let (_d, c) = test_conn();
        insert_legacy_unknown_stub(&c, "10.0.0.1");
        set_trust(&c, "unknown", Some(true), None).unwrap();
        upsert_peer(&c, REAL, "host-i", "10.0.0.1", 12002, Some("fp"), "ca-pem").unwrap();
        cleanup_unknown_stub_at(&c, "10.0.0.1").unwrap();
        let ids = active_ids(&c);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(REAL));
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
        insert_legacy_unknown_stub(&c, "10.0.0.1");
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
        upsert_peer(&c, REAL, "host-i", "10.0.0.1", 12002, Some("fp"), "ca-pem").unwrap();
        cleanup_unknown_stub_at(&c, "10.0.0.1").unwrap();
        let peers = list_peers(&c).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, REAL);
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
        upsert_peer(&c, HOSTG, "host-g", "10.0.0.5", 12002, None, "ca-pem").unwrap();
        set_trust(&c, HOSTG, Some(true), Some(true)).unwrap();
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
