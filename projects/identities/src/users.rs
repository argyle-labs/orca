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
#[replicate(crate = ::macro_runtime, table = "users", lww = "updated_at", unique = "username_lower")]
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
    // Re-creating a previously-deleted username: supersede any delete op so the
    // resurrected account is not swept on the next sync (no-op otherwise). Keyed
    // by the natural key (username_lower), same as the delete op.
    db::replication_ops::note_write(
        conn,
        "users",
        "username_lower",
        &username_lower,
        utils::time::now_millis_since_epoch(),
    )?;
    db::replicate::notify_write("users");
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
        db::replicate::notify_write("users");
    }
    Ok(n > 0)
}

pub fn count(conn: &Connection) -> Result<i64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    Ok(n)
}

pub fn count_admins(conn: &Connection) -> Result<i64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM users WHERE role = 'admin'", [], |r| {
        r.get(0)
    })?;
    Ok(n)
}

pub fn delete_by_id(conn: &Connection, id: &str) -> Result<bool> {
    // Capture the natural key before deleting — the command-log op is keyed by
    // username_lower so peers (which may hold a divergent local `id` for the
    // same account) apply the delete to the right row.
    let username_lower: Option<String> = conn
        .query_row(
            "SELECT username_lower FROM users WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    let n = conn.execute("DELETE FROM users WHERE id = ?1", params![id])?;
    if n > 0 {
        if let Some(key) = username_lower {
            db::replication_ops::note_delete(
                conn,
                "users",
                "username_lower",
                &key,
                utils::time::now_millis_since_epoch(),
            )?;
        }
        db::replicate::notify_write("users");
    }
    Ok(n > 0)
}

/// Lightweight listing carrying `updated_at` (which `User` does not expose).
/// Returns (id, username, role, updated_at) ordered by created_at ASC.
pub fn list_full(conn: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, username, role, updated_at
         FROM users ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
    use db::testing::test_conn;

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
        let bundle = db::replicate::export_all(&src).unwrap();
        assert!(
            bundle.contains_key("users"),
            "users entity must be registered"
        );

        let dst = test_conn();
        let merged = db::replicate::merge_bundle(&dst, bundle).unwrap();
        assert_eq!(merged, 1);
        let got = find_auth_by_username(&dst, "scott").unwrap().unwrap();
        assert_eq!(got.id, "u1");
        assert_eq!(got.password_hash, "hash-v1");
        assert_eq!(got.role, "admin");

        // A newer write (bumped updated_at via password change) propagates.
        set_password_hash(&src, "u1", "hash-v2", "2026-02-01T00:00:00Z").unwrap();
        let n =
            db::replicate::merge_bundle(&dst, db::replicate::export_all(&src).unwrap()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            find_auth_by_username(&dst, "scott")
                .unwrap()
                .unwrap()
                .password_hash,
            "hash-v2"
        );

        // Re-merging the same (now stale) bundle is a no-op — LWW guards it.
        let n2 =
            db::replicate::merge_bundle(&dst, db::replicate::export_all(&src).unwrap()).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn cross_row_unique_poison_is_skipped_not_fatal() {
        // The poison shape that used to storm the fleet: an incoming row whose
        // pk matches one local row while its username belongs to ANOTHER local
        // row — a cross-row UNIQUE clash `ON CONFLICT` cannot resolve. It must
        // be skipped loudly WITHOUT aborting the merge, so clean rows in the
        // same bundle still land.
        let dst = test_conn();
        insert(&dst, "X", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        insert(&dst, "Y", "bob", "h", "member", "2026-01-01T00:00:00Z").unwrap();

        // Source bundle: a NEWER row (id=Y, username=scott) that collides across
        // rows, plus a clean unrelated row (id=Z, username=carol).
        let src = test_conn();
        insert(&src, "Y", "scott", "h2", "admin", "2026-03-01T00:00:00Z").unwrap();
        insert(&src, "Z", "carol", "h", "member", "2026-03-01T00:00:00Z").unwrap();
        let bundle = db::replicate::export_all(&src).unwrap();

        // Must NOT error, and the clean row must merge despite the poison row.
        let merged = db::replicate::merge_bundle(&dst, bundle).unwrap();
        assert_eq!(merged, 1, "clean row lands; poison row skipped");
        assert!(
            find_auth_by_username(&dst, "carol").unwrap().is_some(),
            "clean row past the poison must be applied"
        );
        // The poison was skipped — the original scott row keeps its local id.
        assert_eq!(
            find_auth_by_username(&dst, "scott").unwrap().unwrap().id,
            "X",
            "poison row skipped, local row untouched"
        );
    }

    #[test]
    fn delete_records_op_keyed_by_username_lower() {
        let conn = test_conn();
        insert(&conn, "u1", "Scott", "h", "admin", "t0").unwrap();
        assert!(delete_by_id(&conn, "u1").unwrap());
        assert!(find_by_id(&conn, "u1").unwrap().is_none(), "row is gone");
        let (entity, op): (String, String) = conn
            .query_row(
                "SELECT entity, op FROM replication_ops WHERE key_val = 'scott'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(entity, "users");
        assert_eq!(op, "delete");
    }

    #[test]
    fn recreate_after_delete_supersedes_the_delete_op() {
        let conn = test_conn();
        insert(&conn, "u1", "scott", "h1", "admin", "t0").unwrap();
        delete_by_id(&conn, "u1").unwrap();
        // Same username, fresh id — divergent-id re-create.
        insert(&conn, "u2", "scott", "h2", "admin", "t1").unwrap();
        let op: String = conn
            .query_row(
                "SELECT op FROM replication_ops WHERE key_val = 'scott'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(op, "upsert", "re-create flips the tombstone");
        db::replication_ops::apply_pending_deletes(&conn).unwrap();
        let got = find_auth_by_username(&conn, "scott").unwrap().unwrap();
        assert_eq!(got.id, "u2", "resurrected account survives");
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
