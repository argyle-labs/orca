//! Pod-mesh DB helpers. All operations target the tables added in the v1
//! baseline squash (apply_schema): pod_invites, pod_peers, pod_trust, pod_self.
//!
//! Token-hash storage: invite tokens are `sha256(raw_token)` only — the raw
//! token only exists in the invite blob that crosses the wire, never on disk.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// SHA-256 of a raw invite token, hex-encoded (lowercase, 64 chars).
pub fn hash_token(raw: &str) -> String {
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

// ── pod_invites ──────────────────────────────────────────────────────────────

pub fn insert_invite(
    conn: &Connection,
    token_hash: &str,
    ttl_secs: i64,
    issued_by_cn: &str,
) -> Result<i64> {
    let now = now_secs();
    let expires = now + ttl_secs;
    conn.execute(
        "INSERT INTO pod_invites (token_hash, expires_at, used_at, created_at, issued_by_cn)
         VALUES (?, ?, NULL, ?, ?)",
        params![token_hash, expires, now, issued_by_cn],
    )?;
    Ok(expires)
}

/// Atomically redeem an invite: returns Ok(true) only if the invite existed,
/// hadn't been used, and hadn't expired. Marks it used in the same transaction.
pub fn redeem_invite(conn: &Connection, token_hash: &str) -> Result<bool> {
    let now = now_secs();
    let updated = conn.execute(
        "UPDATE pod_invites
         SET used_at = ?
         WHERE token_hash = ?
           AND used_at IS NULL
           AND expires_at >= ?",
        params![now, token_hash, now],
    )?;
    Ok(updated > 0)
}

// ── pod_peers ────────────────────────────────────────────────────────────────

pub fn upsert_peer(
    conn: &Connection,
    peer_id: &str,
    peer_hostname: &str,
    ca_cert_pem: &str,
) -> Result<()> {
    let now = now_secs();
    conn.execute(
        "INSERT INTO pod_peers (peer_id, peer_hostname, ca_cert_pem, first_seen_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(peer_id) DO UPDATE SET
             peer_hostname = excluded.peer_hostname,
             last_seen_at  = excluded.last_seen_at",
        params![peer_id, peer_hostname, ca_cert_pem, now, now],
    )?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerRow {
    pub peer_id: String,
    pub peer_hostname: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub local_secure: bool,
    pub peer_secure: bool,
}

pub fn list_peers(conn: &Connection) -> Result<Vec<PeerRow>> {
    let mut stmt = conn.prepare(
        "SELECT p.peer_id, p.peer_hostname, p.first_seen_at, p.last_seen_at,
                COALESCE(t.local_secure, 0), COALESCE(t.peer_secure, 0)
         FROM pod_peers p
         LEFT JOIN pod_trust t ON t.peer_id = p.peer_id
         ORDER BY p.last_seen_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PeerRow {
            peer_id: r.get(0)?,
            peer_hostname: r.get(1)?,
            first_seen_at: r.get(2)?,
            last_seen_at: r.get(3)?,
            local_secure: r.get::<_, i64>(4)? != 0,
            peer_secure: r.get::<_, i64>(5)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
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

/// Update the local_secure or peer_secure bit. Upserts the row.
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

/// True if both sides have flagged each other secure — the trigger for
/// CA-key replication.
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
    fn invite_round_trip() {
        let (_dir, conn) = test_conn();
        let h = hash_token("abc123");
        insert_invite(&conn, &h, 60, "founder@mint").unwrap();
        assert!(redeem_invite(&conn, &h).unwrap());
        // second redeem fails
        assert!(!redeem_invite(&conn, &h).unwrap());
    }

    #[test]
    fn invite_expired_cannot_redeem() {
        let (_dir, conn) = test_conn();
        let h = hash_token("expired");
        insert_invite(&conn, &h, -10, "founder@mint").unwrap();
        assert!(!redeem_invite(&conn, &h).unwrap());
    }

    #[test]
    fn peer_upsert_and_list() {
        let (_dir, conn) = test_conn();
        upsert_peer(&conn, "peer.thor", "thor", "ca-pem").unwrap();
        let peers = list_peers(&conn).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].peer_id, "peer.thor");
        assert!(!peers[0].local_secure);
        assert!(!peers[0].peer_secure);
    }

    #[test]
    fn trust_bits_independent() {
        let (_dir, conn) = test_conn();
        upsert_peer(&conn, "peer.thor", "thor", "ca-pem").unwrap();
        set_trust(&conn, "peer.thor", Some(true), None).unwrap();
        let t = get_trust(&conn, "peer.thor").unwrap();
        assert!(t.local_secure);
        assert!(!t.peer_secure);
        assert!(!is_mutual_secure(t));

        set_trust(&conn, "peer.thor", None, Some(true)).unwrap();
        let t = get_trust(&conn, "peer.thor").unwrap();
        assert!(is_mutual_secure(t));
    }

    #[test]
    fn self_secure_defaults_false() {
        let (_dir, conn) = test_conn();
        assert!(!get_self_secure(&conn).unwrap());
        set_self_secure(&conn, true).unwrap();
        assert!(get_self_secure(&conn).unwrap());
    }
}
