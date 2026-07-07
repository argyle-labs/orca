//! Feature flags — typed boolean toggles backed by the `feature_flags` table.
//!
//! Promoted out of the generic `settings` K/V store so the schema can enforce
//! the value domain (0|1) instead of every reader parsing a free-form TEXT
//! column. New flags should land here, not in `settings`.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

pub fn get(conn: &Connection, name: &str) -> Result<Option<bool>> {
    conn.query_row(
        "SELECT enabled FROM feature_flags WHERE name = ?1",
        rusqlite::params![name],
        |row| row.get::<_, bool>(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn set(conn: &Connection, name: &str, enabled: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO feature_flags (name, enabled, updated_at)
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
         ON CONFLICT(name) DO UPDATE SET
             enabled    = excluded.enabled,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![name, enabled],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn seeded_defaults_present() {
        let conn = test_conn();
        assert_eq!(get(&conn, "fs.allow_unrestricted").unwrap(), Some(false));
        assert_eq!(get(&conn, "ui.enabled").unwrap(), Some(true));
        assert_eq!(
            get(&conn, "auth.public_signup_enabled").unwrap(),
            Some(false)
        );
    }

    #[test]
    fn get_missing_returns_none() {
        let conn = test_conn();
        assert_eq!(get(&conn, "no.such.flag").unwrap(), None);
    }

    #[test]
    fn set_then_get_roundtrip() {
        let conn = test_conn();
        set(&conn, "fs.allow_unrestricted", true).unwrap();
        assert_eq!(get(&conn, "fs.allow_unrestricted").unwrap(), Some(true));
        set(&conn, "fs.allow_unrestricted", false).unwrap();
        assert_eq!(get(&conn, "fs.allow_unrestricted").unwrap(), Some(false));
    }

    #[test]
    fn set_inserts_new_flag() {
        let conn = test_conn();
        set(&conn, "experimental.cool_thing", true).unwrap();
        assert_eq!(get(&conn, "experimental.cool_thing").unwrap(), Some(true));
    }

    #[test]
    fn check_constraint_rejects_non_boolean() {
        let conn = test_conn();
        let err = conn.execute(
            "INSERT INTO feature_flags (name, enabled, updated_at)
             VALUES ('bad', 2, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))",
            [],
        );
        assert!(err.is_err(), "CHECK constraint should reject enabled=2");
    }
}
