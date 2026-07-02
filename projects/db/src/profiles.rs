//! Profiles + profile shares + active-profile pointer.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct ProfileRow {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ShareRow {
    pub profile_id: String,
    pub user_id: String,
    pub role: String,
    pub shared_at: String,
}

/// Insert a new profile. Returns Err if (owner_user_id, name) is taken.
pub fn create(
    conn: &Connection,
    id: &str,
    name: &str,
    owner_user_id: &str,
    description: Option<&str>,
) -> Result<ProfileRow> {
    conn.execute(
        "INSERT INTO profiles (id, name, owner_user_id, description) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, name, owner_user_id, description],
    )?;
    get(conn, id)?.ok_or_else(|| anyhow::anyhow!("profile vanished after insert: {id}"))
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at
         FROM profiles WHERE id = ?1",
    )?;
    let row = stmt
        .query_row([id], |r| {
            Ok(ProfileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                owner_user_id: r.get(2)?,
                description: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// Find a profile owned by `owner_user_id` with the given `name`.
pub fn get_by_owner_and_name(
    conn: &Connection,
    owner_user_id: &str,
    name: &str,
) -> Result<Option<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at
         FROM profiles WHERE owner_user_id = ?1 AND name = ?2",
    )?;
    let row = stmt
        .query_row(rusqlite::params![owner_user_id, name], |r| {
            Ok(ProfileRow {
                id: r.get(0)?,
                name: r.get(1)?,
                owner_user_id: r.get(2)?,
                description: r.get(3)?,
                created_at: r.get(4)?,
                updated_at: r.get(5)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// All profiles a user can access — owned + shared (any role).
pub fn list_for_user(conn: &Connection, user_id: &str) -> Result<Vec<ProfileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, owner_user_id, description, created_at, updated_at FROM profiles
         WHERE owner_user_id = ?1
            OR id IN (SELECT profile_id FROM profile_shares WHERE user_id = ?1)
         ORDER BY name",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(ProfileRow {
            id: r.get(0)?,
            name: r.get(1)?,
            owner_user_id: r.get(2)?,
            description: r.get(3)?,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn update(
    conn: &Connection,
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE profiles
            SET name        = COALESCE(?2, name),
                description = COALESCE(?3, description),
                updated_at  = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
          WHERE id = ?1",
        rusqlite::params![id, name, description],
    )?;
    Ok(n > 0)
}

pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
    Ok(n > 0)
}

/// Share or update a share role. Owner cannot self-share (caller enforces).
pub fn share(conn: &Connection, profile_id: &str, user_id: &str, role: &str) -> Result<()> {
    if role != "viewer" && role != "collaborator" {
        return Err(anyhow::anyhow!(
            "invalid role '{role}' (expected 'viewer' or 'collaborator')"
        ));
    }
    conn.execute(
        "INSERT INTO profile_shares (profile_id, user_id, role) VALUES (?1, ?2, ?3)
         ON CONFLICT(profile_id, user_id) DO UPDATE SET
             role      = excluded.role,
             shared_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![profile_id, user_id, role],
    )?;
    Ok(())
}

pub fn unshare(conn: &Connection, profile_id: &str, user_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM profile_shares WHERE profile_id = ?1 AND user_id = ?2",
        rusqlite::params![profile_id, user_id],
    )?;
    Ok(n > 0)
}

pub fn list_shares(conn: &Connection, profile_id: &str) -> Result<Vec<ShareRow>> {
    let mut stmt = conn.prepare(
        "SELECT profile_id, user_id, role, shared_at
         FROM profile_shares WHERE profile_id = ?1 ORDER BY user_id",
    )?;
    let rows = stmt.query_map([profile_id], |r| {
        Ok(ShareRow {
            profile_id: r.get(0)?,
            user_id: r.get(1)?,
            role: r.get(2)?,
            shared_at: r.get(3)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// Get the role a user has on a profile: 'owner', 'viewer', 'collaborator', or None.
pub fn role_for_user(conn: &Connection, profile_id: &str, user_id: &str) -> Result<Option<String>> {
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner_user_id FROM profiles WHERE id = ?1",
            [profile_id],
            |r| r.get(0),
        )
        .optional()?;
    if owner.as_deref() == Some(user_id) {
        return Ok(Some("owner".to_string()));
    }
    let role: Option<String> = conn
        .query_row(
            "SELECT role FROM profile_shares WHERE profile_id = ?1 AND user_id = ?2",
            rusqlite::params![profile_id, user_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(role)
}

pub fn set_active(conn: &Connection, user_id: &str, profile_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO user_active_profile (user_id, profile_id) VALUES (?1, ?2)
         ON CONFLICT(user_id) DO UPDATE SET
             profile_id = excluded.profile_id,
             updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')",
        rusqlite::params![user_id, profile_id],
    )?;
    Ok(())
}

pub fn get_active(conn: &Connection, user_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT profile_id FROM user_active_profile WHERE user_id = ?1",
        [user_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_conn;

    #[test]
    fn create_get_roundtrip() {
        let conn = test_conn();
        let p = create(&conn, "p1", "home", "u1", Some("home lab")).unwrap();
        assert_eq!(p.id, "p1");
        assert_eq!(p.name, "home");
        assert_eq!(p.owner_user_id, "u1");
        assert_eq!(p.description.as_deref(), Some("home lab"));
        assert!(!p.created_at.is_empty());

        let fetched = get(&conn, "p1").unwrap().unwrap();
        assert_eq!(fetched.name, "home");
        assert!(get(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn duplicate_owner_name_rejected() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        assert!(create(&conn, "p2", "home", "u1", None).is_err());
        // Same name under a different owner is fine.
        create(&conn, "p3", "home", "u2", None).unwrap();
    }

    #[test]
    fn get_by_owner_and_name() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        let found = super::get_by_owner_and_name(&conn, "u1", "home")
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "p1");
        assert!(
            super::get_by_owner_and_name(&conn, "u2", "home")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn list_for_user_includes_owned_and_shared() {
        let conn = test_conn();
        create(&conn, "p1", "alpha", "u1", None).unwrap();
        create(&conn, "p2", "beta", "u2", None).unwrap();
        create(&conn, "p3", "gamma", "u2", None).unwrap();
        share(&conn, "p2", "u1", "viewer").unwrap();

        let visible = list_for_user(&conn, "u1").unwrap();
        let ids: Vec<&str> = visible.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p2"]); // ordered by name: alpha, beta
    }

    #[test]
    fn update_coalesces_fields() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", Some("desc")).unwrap();
        assert!(update(&conn, "p1", Some("renamed"), None).unwrap());
        let p = get(&conn, "p1").unwrap().unwrap();
        assert_eq!(p.name, "renamed");
        assert_eq!(p.description.as_deref(), Some("desc")); // untouched
        assert!(!update(&conn, "missing", Some("x"), None).unwrap());
    }

    #[test]
    fn delete_cascades_shares_and_active_pointer() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        share(&conn, "p1", "u2", "collaborator").unwrap();
        set_active(&conn, "u2", "p1").unwrap();

        assert!(delete(&conn, "p1").unwrap());
        assert!(!delete(&conn, "p1").unwrap());
        assert!(list_shares(&conn, "p1").unwrap().is_empty());
        assert!(get_active(&conn, "u2").unwrap().is_none());
    }

    #[test]
    fn share_validates_role_and_upserts() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        assert!(share(&conn, "p1", "u2", "admin").is_err());

        share(&conn, "p1", "u2", "viewer").unwrap();
        share(&conn, "p1", "u2", "collaborator").unwrap(); // upsert, not dup
        let shares = list_shares(&conn, "p1").unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].role, "collaborator");

        assert!(unshare(&conn, "p1", "u2").unwrap());
        assert!(!unshare(&conn, "p1", "u2").unwrap());
    }

    #[test]
    fn role_for_user_precedence() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        share(&conn, "p1", "u2", "viewer").unwrap();

        assert_eq!(
            role_for_user(&conn, "p1", "u1").unwrap().as_deref(),
            Some("owner")
        );
        assert_eq!(
            role_for_user(&conn, "p1", "u2").unwrap().as_deref(),
            Some("viewer")
        );
        assert!(role_for_user(&conn, "p1", "u3").unwrap().is_none());
        assert!(role_for_user(&conn, "missing", "u1").unwrap().is_none());
    }

    #[test]
    fn active_profile_pointer_upserts() {
        let conn = test_conn();
        create(&conn, "p1", "home", "u1", None).unwrap();
        create(&conn, "p2", "work", "u1", None).unwrap();

        assert!(get_active(&conn, "u1").unwrap().is_none());
        set_active(&conn, "u1", "p1").unwrap();
        assert_eq!(get_active(&conn, "u1").unwrap().as_deref(), Some("p1"));
        set_active(&conn, "u1", "p2").unwrap();
        assert_eq!(get_active(&conn, "u1").unwrap().as_deref(), Some("p2"));
    }
}
