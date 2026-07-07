//! REST/MCP API bearer token storage.
//!
//! Rows store `sha256(plaintext)` only — the raw token is returned exactly
//! once from `auth.token_create` and is unrecoverable from the DB. Lookups
//! by hash are O(1) via `idx_api_tokens_hash`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub role: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    /// Issuing user. `None` on legacy rows minted before user-binding
    /// (pre-2026-05-29) — those tokens can't produce a CallerIdentity for
    /// remote pod/exec dispatch.
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiTokenLookup {
    pub id: String,
    pub name: String,
    pub role: String,
    pub expires_at: Option<String>,
    pub user_id: Option<String>,
}

/// Insert a new token row. `token_hash` must be the lowercase-hex sha256 of
/// the plaintext token. `user_id` is the authenticated operator who minted
/// this token — used to derive a CallerIdentity on subsequent bearer-auth
/// requests. Returns the stored summary (without the hash).
#[allow(clippy::too_many_arguments)]
pub fn insert(
    conn: &Connection,
    id: &str,
    name: &str,
    token_hash: &str,
    role: &str,
    created_at: &str,
    expires_at: Option<&str>,
    user_id: Option<&str>,
) -> Result<ApiToken> {
    conn.execute(
        "INSERT INTO api_tokens (id, name, token_hash, role, created_at, expires_at, user_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, name, token_hash, role, created_at, expires_at, user_id],
    )?;
    Ok(ApiToken {
        id: id.to_string(),
        name: name.to_string(),
        role: role.to_string(),
        created_at: created_at.to_string(),
        last_used_at: None,
        expires_at: expires_at.map(|s| s.to_string()),
        user_id: user_id.map(|s| s.to_string()),
    })
}

/// All tokens, newest first. Excludes `token_hash`.
pub fn list(conn: &Connection) -> Result<Vec<ApiToken>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, role, created_at, last_used_at, expires_at, user_id
         FROM api_tokens ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve a token hash to its identity row, or `None` if no match.
pub fn find_by_hash(conn: &Connection, token_hash: &str) -> Result<Option<ApiTokenLookup>> {
    let r = conn
        .query_row(
            "SELECT id, name, role, expires_at, user_id FROM api_tokens WHERE token_hash = ?1",
            params![token_hash],
            |r| {
                Ok(ApiTokenLookup {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    expires_at: r.get(3)?,
                    user_id: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(r)
}

/// Stamp `last_used_at`. Cheap — best-effort, ignore failure at the caller.
pub fn touch(conn: &Connection, id: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE api_tokens SET last_used_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

/// Revoke a token by id. Returns true if a row was deleted.
pub fn revoke(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM api_tokens WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// Total token count. Used by the localhost-bootstrap gate: if zero tokens
/// exist, `/api/auth.token_create` from 127.0.0.1 is allowed without auth.
pub fn count(conn: &Connection) -> Result<i64> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM api_tokens", [], |r| r.get(0))?;
    Ok(n)
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApiToken> {
    Ok(ApiToken {
        id: r.get(0)?,
        name: r.get(1)?,
        role: r.get(2)?,
        created_at: r.get(3)?,
        last_used_at: r.get(4)?,
        expires_at: r.get(5)?,
        user_id: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn insert_lookup_revoke_roundtrip() {
        let conn = test_conn();
        let row = insert(
            &conn,
            "01J000000000000000000000AA",
            "ci",
            "deadbeef",
            "admin",
            "2026-05-15T00:00:00Z",
            None,
            Some("u_alice"),
        )
        .unwrap();
        assert_eq!(row.name, "ci");
        assert_eq!(row.user_id.as_deref(), Some("u_alice"));
        assert_eq!(count(&conn).unwrap(), 1);

        let hit = find_by_hash(&conn, "deadbeef").unwrap().unwrap();
        assert_eq!(hit.id, row.id);
        assert_eq!(hit.role, "admin");
        assert_eq!(hit.user_id.as_deref(), Some("u_alice"));

        touch(&conn, &row.id, "2026-05-15T00:00:01Z").unwrap();
        let after = list(&conn).unwrap();
        assert_eq!(
            after[0].last_used_at.as_deref(),
            Some("2026-05-15T00:00:01Z")
        );

        assert!(revoke(&conn, &row.id).unwrap());
        assert_eq!(count(&conn).unwrap(), 0);
        assert!(find_by_hash(&conn, "deadbeef").unwrap().is_none());
    }

    #[test]
    fn unique_name_and_hash() {
        let conn = test_conn();
        insert(&conn, "id1", "n", "h1", "admin", "t", None, None).unwrap();
        assert!(insert(&conn, "id2", "n", "h2", "admin", "t", None, None).is_err());
        assert!(insert(&conn, "id3", "n3", "h1", "admin", "t", None, None).is_err());
    }
}
