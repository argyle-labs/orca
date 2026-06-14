//! Web-UI account storage. Mesh-synced across every paired host.
//!
//! Username UNIQUE is case-insensitive — `username_lower` is the canonical key
//! used for every lookup. `username` preserves the original case for display.
//! Password hashes are argon2id (encoded form); this crate stores the string
//! opaquely and leaves verification to the server crate.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub username: String,
    pub role: String,
    pub created_at: String,
    pub password_updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAuth {
    pub id: String,
    pub username: String,
    pub role: String,
    pub password_hash: String,
}

/// Full replicable user row. `users` is ONE shared pool replicated across every
/// paired host (last-write-wins on `updated_at`), so any admin can sign in on
/// any machine/UI. The whole row — including `password_hash` and `role` — is
/// shared among paired peers. See project_unified_mesh_state.md (shared policy).
///
/// Field order mirrors the `users` table columns exactly (the `Replicated`
/// derive maps fields ↔ columns 1:1), so `username_lower` is carried even
/// though it is just `lower(username)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, derive::Replicated)]
#[replicate(crate = ::macro_runtime, table = "users", lww = "updated_at")]
pub struct ReplicaUser {
    pub id: String,
    pub username: String,
    pub username_lower: String,
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
    pub password_updated_at: String,
    pub updated_at: String,
}

pub fn insert(
    conn: &Connection,
    id: &str,
    username: &str,
    password_hash: &str,
    role: &str,
    now: &str,
) -> Result<User> {
    let username_lower = username.to_lowercase();
    conn.execute(
        "INSERT INTO users
            (id, username, username_lower, password_hash, role,
             created_at, password_updated_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)",
        params![id, username, username_lower, password_hash, role, now],
    )?;
    crate::replicate::notify_write("users");
    Ok(User {
        id: id.to_string(),
        username: username.to_string(),
        role: role.to_string(),
        created_at: now.to_string(),
        password_updated_at: now.to_string(),
    })
}

pub fn find_by_id(conn: &Connection, id: &str) -> Result<Option<User>> {
    let r = conn
        .query_row(
            "SELECT id, username, role, created_at, password_updated_at
             FROM users WHERE id = ?1",
            params![id],
            row_user,
        )
        .optional()?;
    Ok(r)
}

/// Case-insensitive username lookup returning the auth-relevant fields,
/// including `password_hash`. Used by `auth.signin` and `auth.reset_password`.
pub fn find_auth_by_username(conn: &Connection, username: &str) -> Result<Option<UserAuth>> {
    let key = username.to_lowercase();
    let r = conn
        .query_row(
            "SELECT id, username, role, password_hash
             FROM users WHERE username_lower = ?1",
            params![key],
            |r| {
                Ok(UserAuth {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    role: r.get(2)?,
                    password_hash: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(r)
}

pub fn set_password_hash(conn: &Connection, id: &str, new_hash: &str, now: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE users SET password_hash = ?2, password_updated_at = ?3, updated_at = ?3 WHERE id = ?1",
        params![id, new_hash, now],
    )?;
    if n > 0 {
        crate::replicate::notify_write("users");
    }
    Ok(n > 0)
}

pub fn count(conn: &Connection) -> Result<i64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    Ok(n)
}

/// The earliest-created admin user, if any. Used to resolve the host's
/// ambient operator identity for minting signed caller tokens on the
/// CLI/daemon remote-dispatch path.
pub fn first_admin(conn: &Connection) -> Result<Option<User>> {
    let r = conn
        .query_row(
            "SELECT id, username, role, created_at, password_updated_at
             FROM users WHERE role = 'admin' ORDER BY created_at ASC LIMIT 1",
            [],
            row_user,
        )
        .optional()?;
    Ok(r)
}

pub fn list(conn: &Connection) -> Result<Vec<User>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, role, created_at, password_updated_at
         FROM users ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], row_user)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn row_user(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        role: r.get(2)?,
        created_at: r.get(3)?,
        password_updated_at: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn case_insensitive_unique_and_lookup() {
        let conn = test_conn();
        insert(&conn, "u1", "Scott", "h1", "admin", "t0").unwrap();
        // Same username different case — must reject.
        assert!(insert(&conn, "u2", "scott", "h2", "member", "t0").is_err());

        // Lookup by any case finds the same row.
        let by_upper = find_auth_by_username(&conn, "SCOTT").unwrap().unwrap();
        let by_mixed = find_auth_by_username(&conn, "ScOtT").unwrap().unwrap();
        assert_eq!(by_upper.id, "u1");
        assert_eq!(by_mixed.id, "u1");
        // Display case is preserved.
        assert_eq!(by_upper.username, "Scott");
    }

    #[test]
    fn replicated_derive_export_then_merge_lww() {
        // Source host has one user; export the shared pool, merge into a fresh
        // host, and confirm the row lands (any host can write the pool).
        let src = test_conn();
        insert(
            &src,
            "u1",
            "Scott",
            "hash-v1",
            "admin",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let bundle = crate::replicate::export_all(&src).unwrap();
        assert!(
            bundle.contains_key("users"),
            "users entity must be registered"
        );

        let dst = test_conn();
        let merged = crate::replicate::merge_bundle(&dst, bundle).unwrap();
        assert_eq!(merged, 1);
        let got = find_auth_by_username(&dst, "scott").unwrap().unwrap();
        assert_eq!(got.id, "u1");
        assert_eq!(got.password_hash, "hash-v1");
        assert_eq!(got.role, "admin");

        // A newer write (bumped updated_at via password change) propagates.
        set_password_hash(&src, "u1", "hash-v2", "2026-02-01T00:00:00Z").unwrap();
        let n = crate::replicate::merge_bundle(&dst, crate::replicate::export_all(&src).unwrap())
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            find_auth_by_username(&dst, "scott")
                .unwrap()
                .unwrap()
                .password_hash,
            "hash-v2"
        );

        // Re-merging the same (now stale) bundle is a no-op — LWW guards it.
        let n2 = crate::replicate::merge_bundle(&dst, crate::replicate::export_all(&src).unwrap())
            .unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn password_update_bumps_timestamp() {
        let conn = test_conn();
        insert(&conn, "u1", "alice", "h1", "member", "t0").unwrap();
        assert!(set_password_hash(&conn, "u1", "h2", "t1").unwrap());
        let u = find_by_id(&conn, "u1").unwrap().unwrap();
        assert_eq!(u.password_updated_at, "t1");
    }
}
