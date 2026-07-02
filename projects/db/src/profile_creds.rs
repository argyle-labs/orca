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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    fn with_profile(conn: &Connection) {
        crate::profiles::create(conn, "p1", "home", "u1", None).unwrap();
    }

    #[test]
    fn set_get_roundtrip_and_upsert() {
        let conn = test_conn();
        with_profile(&conn);
        set(&conn, "p1", "api_key", "secret1").unwrap();
        assert_eq!(
            get(&conn, "p1", "api_key").unwrap().as_deref(),
            Some("secret1")
        );

        set(&conn, "p1", "api_key", "secret2").unwrap();
        assert_eq!(
            get(&conn, "p1", "api_key").unwrap().as_deref(),
            Some("secret2")
        );
        assert!(get(&conn, "p1", "missing").unwrap().is_none());
    }

    #[test]
    fn list_is_sorted_and_scoped() {
        let conn = test_conn();
        with_profile(&conn);
        crate::profiles::create(&conn, "p2", "work", "u1", None).unwrap();
        set(&conn, "p1", "b_key", "1").unwrap();
        set(&conn, "p1", "a_key", "2").unwrap();
        set(&conn, "p2", "other", "3").unwrap();

        assert_eq!(list(&conn, "p1").unwrap(), vec!["a_key", "b_key"]);
        assert_eq!(list(&conn, "p2").unwrap(), vec!["other"]);
    }

    #[test]
    fn delete_and_profile_cascade() {
        let conn = test_conn();
        with_profile(&conn);
        set(&conn, "p1", "k", "v").unwrap();
        assert!(delete(&conn, "p1", "k").unwrap());
        assert!(!delete(&conn, "p1", "k").unwrap());

        set(&conn, "p1", "k2", "v").unwrap();
        crate::profiles::delete(&conn, "p1").unwrap();
        assert!(list(&conn, "p1").unwrap().is_empty());
    }
}
