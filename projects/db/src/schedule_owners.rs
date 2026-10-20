//! Owner of each locally saved schedule row, bound to the row's exact payload.
//!
//! LOCAL ONLY — never register this table for replication. A schedule row that
//! arrives over the mesh or a restore has no matching entry here (or a digest
//! that no longer matches), so the scheduler refuses it until it is re-saved
//! locally by a verified caller.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleOwner {
    pub name: String,
    pub user_id: String,
    pub role: String,
    pub row_sha256: String,
}

pub fn digest(host_owner: &str, name: &str, json: &str) -> String {
    utils::hash::sha256_hex(format!("{host_owner}\0schedule\0{name}\0{json}").as_bytes())
}

pub fn set(conn: &Connection, owner: &ScheduleOwner) -> Result<()> {
    conn.execute(
        "INSERT INTO schedule_owners (name, user_id, role, row_sha256) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
             user_id    = excluded.user_id,
             role       = excluded.role,
             row_sha256 = excluded.row_sha256",
        params![owner.name, owner.user_id, owner.role, owner.row_sha256],
    )?;
    Ok(())
}

pub fn get(conn: &Connection, name: &str) -> Result<Option<ScheduleOwner>> {
    Ok(conn
        .query_row(
            "SELECT name, user_id, role, row_sha256 FROM schedule_owners WHERE name = ?1",
            params![name],
            |r| {
                Ok(ScheduleOwner {
                    name: r.get(0)?,
                    user_id: r.get(1)?,
                    role: r.get(2)?,
                    row_sha256: r.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn delete(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM schedule_owners WHERE name = ?1", params![name])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn set_get_delete_roundtrip() {
        let conn = test_conn();
        let owner = ScheduleOwner {
            name: "nightly".into(),
            user_id: "u".into(),
            role: "admin".into(),
            row_sha256: digest("h", "nightly", "{}"),
        };
        set(&conn, &owner).unwrap();
        assert_eq!(get(&conn, "nightly").unwrap(), Some(owner));
        assert_ne!(digest("h", "nightly", "{}"), digest("h", "nightly", "{ }"));
        delete(&conn, "nightly").unwrap();
        assert_eq!(get(&conn, "nightly").unwrap(), None);
    }
}
