//! Settings — generic key/value store backed by the `settings` table.
//!
//! Two paired APIs exist for historical reasons:
//! - `get`/`set`/`delete`/`list_prefix` (the canonical names)
//! - `get_legacy`/`set_legacy`/`list_all` for callers that match the older shape
//!
//! Secrets live in this same SQLCipher-encrypted table under the `secrets.` prefix
//! (see `secret_*` helpers).

use anyhow::Result;
use rusqlite::Connection;

// ── Generic key/value ─────────────────────────────────────────────────────────

pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get::<_, String>(0),
    );
    match result {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, updated_at)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM settings WHERE key = ?1",
        rusqlite::params![key],
    )?;
    Ok(n > 0)
}

pub fn list_prefix(conn: &Connection, prefix: &str) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM settings WHERE key LIKE ?1 ORDER BY key")?;
    let pattern = format!("{prefix}%");
    let rows = stmt.query_map(rusqlite::params![pattern], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// ── Legacy shape (used by older call sites; same table) ──────────────────────

pub fn get_legacy(conn: &Connection, key: &str) -> Result<Option<String>> {
    let val = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            rusqlite::params![key],
            |row| row.get(0),
        )
        .ok();
    Ok(val)
}

pub fn set_legacy(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn list_all(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// ── Secrets (settings rows under the `secrets.` prefix) ──────────────────────
//
// These live in the SQLCipher-encrypted `settings` table, so values are at-rest
// encrypted by the same key the rest of the orca DB uses. Read/write through
// these helpers so the prefix stays consistent and we can later add a separate
// table or audit log without touching call sites.

const SECRET_PREFIX: &str = "secrets.";

pub fn secret_get(conn: &Connection, name: &str) -> Result<Option<String>> {
    get(conn, &format!("{SECRET_PREFIX}{name}"))
}

pub fn secret_set(conn: &Connection, name: &str, value: &str) -> Result<()> {
    set(conn, &format!("{SECRET_PREFIX}{name}"), value)
}

pub fn secret_delete(conn: &Connection, name: &str) -> Result<bool> {
    delete(conn, &format!("{SECRET_PREFIX}{name}"))
}

/// Mask an API key for display: first 8 chars + ellipsis + last 4. Short keys (≤12) are fully masked.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() > 12 {
        let prefix: String = chars[..8].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}…{suffix}")
    } else {
        "****".to_string()
    }
}

/// Returns true if the key looks like an Anthropic key (starts with `sk-ant-`).
pub fn looks_like_anthropic_key(key: &str) -> bool {
    key.starts_with("sk-ant-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn get_set_delete() {
        let conn = test_conn();
        assert!(get(&conn, "my.flag").unwrap().is_none());

        set(&conn, "my.flag", "enabled").unwrap();
        assert_eq!(get(&conn, "my.flag").unwrap().as_deref(), Some("enabled"));

        set(&conn, "my.flag", "disabled").unwrap();
        assert_eq!(get(&conn, "my.flag").unwrap().as_deref(), Some("disabled"));

        assert!(delete(&conn, "my.flag").unwrap());
        assert!(get(&conn, "my.flag").unwrap().is_none());
        assert!(!delete(&conn, "my.flag").unwrap());
    }

    #[test]
    fn list_prefix_filters() {
        let conn = test_conn();
        set(&conn, "foo.a", "1").unwrap();
        set(&conn, "foo.b", "2").unwrap();
        set(&conn, "bar.c", "3").unwrap();

        let foo = list_prefix(&conn, "foo.").unwrap();
        assert_eq!(foo.len(), 2);
        assert!(foo.iter().all(|(k, _)| k.starts_with("foo.")));
    }

    #[test]
    fn secret_uses_settings_prefix() {
        let conn = test_conn();
        secret_set(&conn, "ANTHROPIC_KEY", "sk-ant-test").unwrap();
        let all = list_prefix(&conn, "secrets.").unwrap();
        assert!(all.iter().any(|(k, _)| k == "secrets.ANTHROPIC_KEY"));
        assert_eq!(
            secret_get(&conn, "ANTHROPIC_KEY").unwrap().as_deref(),
            Some("sk-ant-test")
        );
        assert!(secret_delete(&conn, "ANTHROPIC_KEY").unwrap());
        assert!(secret_get(&conn, "ANTHROPIC_KEY").unwrap().is_none());
    }

    #[test]
    fn legacy_and_canonical_both_work() {
        let conn = test_conn();
        set_legacy(&conn, "x", "42").unwrap();
        assert_eq!(get_legacy(&conn, "x").unwrap().as_deref(), Some("42"));
        assert_eq!(get(&conn, "x").unwrap().as_deref(), Some("42"));
    }

    #[test]
    fn mask_key_long_key_shows_first_and_last() {
        let key = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz";
        let masked = mask_key(key);
        assert!(masked.starts_with("sk-ant-a"), "prefix wrong: {masked}");
        assert!(masked.ends_with("wxyz"), "suffix wrong: {masked}");
        assert!(masked.contains('…'), "no ellipsis: {masked}");
    }

    #[test]
    fn mask_key_short_returns_stars() {
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn mask_key_exactly_12_returns_stars() {
        assert_eq!(mask_key("abcdefghijkl"), "****");
    }

    #[test]
    fn mask_key_13_chars_masks() {
        let key = "abcdefghijklm";
        let masked = mask_key(key);
        assert!(masked.starts_with("abcdefgh"), "got: {masked}");
        assert!(masked.ends_with("jklm"), "got: {masked}");
    }

    #[test]
    fn mask_key_empty_returns_stars() {
        assert_eq!(mask_key(""), "****");
    }

    #[test]
    fn looks_like_anthropic_accepts_real_format() {
        assert!(looks_like_anthropic_key("sk-ant-api03-xyz"));
        assert!(!looks_like_anthropic_key("sk-1234"));
        assert!(!looks_like_anthropic_key(""));
    }
}
