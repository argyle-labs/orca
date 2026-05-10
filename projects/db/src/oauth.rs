//! OAuth token storage — per-service access/refresh tokens persisted in orca.db.

use anyhow::Result;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct TokenRow {
    pub service: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
}

pub fn upsert(conn: &Connection, row: &TokenRow) -> Result<()> {
    conn.execute(
        "INSERT INTO oauth_tokens (service, access_token, refresh_token, expires_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(service) DO UPDATE SET
             access_token  = excluded.access_token,
             refresh_token = excluded.refresh_token,
             expires_at    = excluded.expires_at,
             updated_at    = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![
            row.service,
            row.access_token,
            row.refresh_token,
            row.expires_at
        ],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, service: &str) -> Result<Option<TokenRow>> {
    let mut stmt = conn.prepare(
        "SELECT service, access_token, refresh_token, expires_at
         FROM oauth_tokens WHERE service = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![service], |row| {
        Ok(TokenRow {
            service: row.get(0)?,
            access_token: row.get(1)?,
            refresh_token: row.get(2)?,
            expires_at: row.get(3)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn delete(conn: &Connection, service: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM oauth_tokens WHERE service = ?1",
        rusqlite::params![service],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn round_trip() {
        let conn = test_conn();
        assert!(get(&conn, "github").unwrap().is_none());

        let row = TokenRow {
            service: "github".into(),
            access_token: "gha_abc".into(),
            refresh_token: Some("refresh_xyz".into()),
            expires_at: Some("2027-01-01T00:00:00Z".into()),
        };
        upsert(&conn, &row).unwrap();

        let found = get(&conn, "github").unwrap().unwrap();
        assert_eq!(found.access_token, "gha_abc");
        assert_eq!(found.refresh_token.as_deref(), Some("refresh_xyz"));

        // Upsert updates
        let row2 = TokenRow {
            service: "github".into(),
            access_token: "new_token".into(),
            refresh_token: None,
            expires_at: None,
        };
        upsert(&conn, &row2).unwrap();
        let found2 = get(&conn, "github").unwrap().unwrap();
        assert_eq!(found2.access_token, "new_token");
        assert!(found2.refresh_token.is_none());

        assert!(delete(&conn, "github").unwrap());
        assert!(get(&conn, "github").unwrap().is_none());
        assert!(!delete(&conn, "github").unwrap());
    }
}
