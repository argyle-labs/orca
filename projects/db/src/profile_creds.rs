//! Profile credentials — encrypted KV store scoped to a profile.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub fn set(conn: &Connection, profile_id: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO profile_credentials (profile_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(profile_id, key) DO UPDATE SET
             value      = excluded.value,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![profile_id, key, value],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, profile_id: &str, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM profile_credentials WHERE profile_id = ?1 AND key = ?2",
        rusqlite::params![profile_id, key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn list(conn: &Connection, profile_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT key FROM profile_credentials WHERE profile_id = ?1 ORDER BY key")?;
    let rows = stmt.query_map([profile_id], |r| r.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn delete(conn: &Connection, profile_id: &str, key: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM profile_credentials WHERE profile_id = ?1 AND key = ?2",
        rusqlite::params![profile_id, key],
    )?;
    Ok(n > 0)
}
