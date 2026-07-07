//! Browser cookie sessions for the web UI.
//!
//! Cookie carries `session.id` (32 random bytes, hex). Lookup joins to `users`.
//! Sliding 30-day expiry: every authenticated request calls `touch` which
//! refreshes `last_used_at` AND `expires_at = now + 30d`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: String,
    pub created_at: String,
    pub last_used_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionWithUser {
    pub session_id: String,
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub expires_at: String,
    /// Data-mutation opt-in for this browser session (mirror of
    /// `api_tokens.can_mutate`). Default false.
    pub can_mutate: bool,
}

pub fn insert(
    conn: &Connection,
    id: &str,
    user_id: &str,
    now: &str,
    expires_at: &str,
) -> Result<Session> {
    conn.execute(
        "INSERT INTO sessions (id, user_id, created_at, last_used_at, expires_at)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        params![id, user_id, now, expires_at],
    )?;
    Ok(Session {
        id: id.to_string(),
        user_id: user_id.to_string(),
        created_at: now.to_string(),
        last_used_at: now.to_string(),
        expires_at: expires_at.to_string(),
    })
}

/// Resolve a cookie session id to (user_id, username, role, expires_at).
/// Excludes revoked rows and expired rows (caller compares `expires_at`
/// to wall-clock, but the SQL already filters by `revoked_at IS NULL`).
pub fn find_active(conn: &Connection, id: &str) -> Result<Option<SessionWithUser>> {
    let r = conn
        .query_row(
            "SELECT s.id, s.user_id, u.username, u.role, s.expires_at, s.can_mutate
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.id = ?1 AND s.revoked_at IS NULL",
            params![id],
            |r| {
                Ok(SessionWithUser {
                    session_id: r.get(0)?,
                    user_id: r.get(1)?,
                    username: r.get(2)?,
                    role: r.get(3)?,
                    expires_at: r.get(4)?,
                    can_mutate: r.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(r)
}

/// Refresh both `last_used_at` and `expires_at` (sliding expiry).
pub fn touch(conn: &Connection, id: &str, now: &str, new_expires_at: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET last_used_at = ?2, expires_at = ?3
         WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now, new_expires_at],
    )?;
    Ok(())
}

pub fn revoke(conn: &Connection, id: &str, now: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE sessions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now],
    )?;
    Ok(n > 0)
}

/// Revoke every active session for a user — used by `auth reset_password
/// --revoke-sessions` and by self-initiated "sign out everywhere".
pub fn revoke_all_for_user(conn: &Connection, user_id: &str, now: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE sessions SET revoked_at = ?2
         WHERE user_id = ?1 AND revoked_at IS NULL",
        params![user_id, now],
    )?;
    Ok(n)
}

/// Hard-delete revoked or long-expired rows. Cheap maintenance, optional.
pub fn prune(conn: &Connection, cutoff: &str) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM sessions WHERE expires_at < ?1 OR revoked_at IS NOT NULL",
        params![cutoff],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn seed_user(conn: &Connection) -> String {
        crate::users::insert(conn, "u1", "alice", "$h$", "admin", "2026-05-15T00:00:00Z")
            .unwrap()
            .id
    }

    #[test]
    fn create_lookup_slide_revoke() {
        let conn = test_conn();
        let uid = seed_user(&conn);

        insert(&conn, "s1", &uid, "t0", "t30").unwrap();
        let hit = find_active(&conn, "s1").unwrap().unwrap();
        assert_eq!(hit.user_id, uid);
        assert_eq!(hit.username, "alice");

        // Sliding refresh.
        touch(&conn, "s1", "t1", "t31").unwrap();
        let after = find_active(&conn, "s1").unwrap().unwrap();
        assert_eq!(after.expires_at, "t31");

        assert!(revoke(&conn, "s1", "t2").unwrap());
        assert!(find_active(&conn, "s1").unwrap().is_none());
    }

    #[test]
    fn revoke_all_for_user_clears_every_active() {
        let conn = test_conn();
        let uid = seed_user(&conn);
        insert(&conn, "s1", &uid, "t0", "t30").unwrap();
        insert(&conn, "s2", &uid, "t0", "t30").unwrap();
        insert(&conn, "s3", &uid, "t0", "t30").unwrap();
        assert_eq!(revoke_all_for_user(&conn, &uid, "t1").unwrap(), 3);
        assert!(find_active(&conn, "s1").unwrap().is_none());
        // Idempotent — second call revokes zero.
        assert_eq!(revoke_all_for_user(&conn, &uid, "t2").unwrap(), 0);
    }
}
