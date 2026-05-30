//! Per-user, shareable profiles.
//!
//! A profile owns filesystem content (`~/.orca/profiles/<id>/`) and metadata in
//! the encrypted DB (`orca.db`). One user has many profiles; profiles can be
//! shared with other users in `viewer` or `collaborator` roles.
//!
//! v1 is single-machine. The data model is federation-ready (UUID ids,
//! set-shaped ACLs, file-granular content) so the pod mesh sync layer can
//! replicate without re-shaping.
//!
//! See `project_profile_path.md` for the design.

use anyhow::{Result, anyhow};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Permission a non-owner user has on a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Read-only consumer.
    Viewer,
    /// Can edit content. Cannot change ownership or sharing.
    Collaborator,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Collaborator => "collaborator",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Role::Viewer),
            "collaborator" => Some(Role::Collaborator),
            _ => None,
        }
    }
}

/// Effective access a user has on a profile (owner, role, or none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Owner,
    Viewer,
    Collaborator,
    None,
}

impl Access {
    /// Is read access permitted?
    pub fn can_read(self) -> bool {
        !matches!(self, Access::None)
    }
    /// Is write access permitted?
    pub fn can_write(self) -> bool {
        matches!(self, Access::Owner | Access::Collaborator)
    }
    /// Is admin (delete, share-management, ownership) permitted?
    pub fn can_admin(self) -> bool {
        matches!(self, Access::Owner)
    }
}

#[derive(Debug, Error)]
pub enum NamespaceError {
    #[error("namespace not found: {0}")]
    NotFound(String),
    #[error("permission denied for user {user} on namespace {namespace}")]
    PermissionDenied { user: String, namespace: String },
    #[error("name '{0}' already taken for this owner")]
    NameTaken(String),
    #[error("invalid role: {0}")]
    InvalidRole(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A profile's identity + metadata.
#[derive(Debug, Clone)]
pub struct Namespace {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub description: Option<String>,
    pub root: PathBuf,
}

impl Namespace {
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }
    pub fn plugins_toml(&self) -> PathBuf {
        self.root.join("plugins.toml")
    }
    pub fn hosts_toml(&self) -> PathBuf {
        self.root.join("hosts.toml")
    }
    pub fn contexts_dir(&self) -> PathBuf {
        self.root.join("contexts")
    }
    pub fn dashboards_dir(&self) -> PathBuf {
        self.root.join("dashboards")
    }

    fn from_row(row: db::profiles::ProfileRow, namespaces_root: &Path) -> Self {
        let root = namespaces_root.join(&row.id);
        Self {
            id: row.id,
            name: row.name,
            owner_user_id: row.owner_user_id,
            description: row.description,
            root,
        }
    }

    /// Create the on-disk directory layout for this profile (idempotent).
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.agents_dir())?;
        std::fs::create_dir_all(self.contexts_dir())?;
        std::fs::create_dir_all(self.dashboards_dir())?;
        Ok(())
    }
}

/// Manager for profile lifecycle, ACL checks, and active-profile resolution.
///
/// Holds the filesystem root for profile content. Callers pass a DB connection
/// per call so the manager doesn't own connection lifetime.
pub struct NamespaceManager {
    namespaces_root: PathBuf,
}

impl NamespaceManager {
    pub fn new(namespaces_root: PathBuf) -> Self {
        Self { namespaces_root }
    }

    pub fn from_config(cfg: &contract::config::Config) -> Self {
        Self::new(cfg.profiles_dir())
    }

    pub fn namespaces_root(&self) -> &Path {
        &self.namespaces_root
    }

    /// Create a new profile owned by `owner_user_id`. Returns the new profile.
    /// Errors with `NamespaceError::NameTaken` if (owner, name) already exists.
    pub fn create(
        &self,
        conn: &Connection,
        owner_user_id: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Namespace, NamespaceError> {
        if let Some(_existing) = db::profiles::get_by_owner_and_name(conn, owner_user_id, name)
            .map_err(NamespaceError::Other)?
        {
            return Err(NamespaceError::NameTaken(name.to_string()));
        }
        let id = Uuid::now_v7().to_string();
        let row = db::profiles::create(conn, &id, name, owner_user_id, description)
            .map_err(NamespaceError::Other)?;
        let profile = Namespace::from_row(row, &self.namespaces_root);
        profile.ensure_dirs().map_err(NamespaceError::Other)?;
        tracing::info!(namespace_id = %profile.id, owner = %owner_user_id, name = %name, "created profile");
        Ok(profile)
    }

    /// Get a profile by id. ACL-checked: errors with `PermissionDenied` if
    /// `requesting_user_id` has no access.
    pub fn get(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
    ) -> Result<Namespace, NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_read() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        let row = db::profiles::get(conn, namespace_id)
            .map_err(NamespaceError::Other)?
            .ok_or_else(|| NamespaceError::NotFound(namespace_id.to_string()))?;
        Ok(Namespace::from_row(row, &self.namespaces_root))
    }

    /// Resolve a profile by name owned by `owner_user_id`. Skips ACL — the
    /// caller is the owner. For looking up someone else's shared profile by
    /// name, callers should use the returned id with `get()`.
    pub fn get_by_owner_and_name(
        &self,
        conn: &Connection,
        owner_user_id: &str,
        name: &str,
    ) -> Result<Option<Namespace>, NamespaceError> {
        let row = db::profiles::get_by_owner_and_name(conn, owner_user_id, name)
            .map_err(NamespaceError::Other)?;
        Ok(row.map(|r| Namespace::from_row(r, &self.namespaces_root)))
    }

    /// All profiles a user can access (owned + shared in any role).
    pub fn list_for_user(
        &self,
        conn: &Connection,
        user_id: &str,
    ) -> Result<Vec<Namespace>, NamespaceError> {
        let rows = db::profiles::list_for_user(conn, user_id).map_err(NamespaceError::Other)?;
        Ok(rows
            .into_iter()
            .map(|r| Namespace::from_row(r, &self.namespaces_root))
            .collect())
    }

    /// Effective access for a user on a profile.
    pub fn access(
        &self,
        conn: &Connection,
        namespace_id: &str,
        user_id: &str,
    ) -> Result<Access, NamespaceError> {
        let role = db::profiles::role_for_user(conn, namespace_id, user_id)
            .map_err(NamespaceError::Other)?;
        Ok(match role.as_deref() {
            Some("owner") => Access::Owner,
            Some("viewer") => Access::Viewer,
            Some("collaborator") => Access::Collaborator,
            _ => Access::None,
        })
    }

    /// Update name/description. Requires admin (owner) access.
    pub fn update(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<(), NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_admin() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        if !db::profiles::update(conn, namespace_id, name, description)
            .map_err(NamespaceError::Other)?
        {
            return Err(NamespaceError::NotFound(namespace_id.to_string()));
        }
        Ok(())
    }

    /// Delete a profile (DB rows + filesystem content). Requires admin.
    pub fn delete(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
    ) -> Result<(), NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_admin() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        if !db::profiles::delete(conn, namespace_id).map_err(NamespaceError::Other)? {
            return Err(NamespaceError::NotFound(namespace_id.to_string()));
        }
        let dir = self.namespaces_root.join(namespace_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| NamespaceError::Other(e.into()))?;
        }
        Ok(())
    }

    /// Share a profile with another user. Requires admin (owner).
    pub fn share(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
        with_user_id: &str,
        role: Role,
    ) -> Result<(), NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_admin() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        if requesting_user_id == with_user_id {
            return Err(NamespaceError::Other(anyhow!(
                "cannot share with self (you are the owner)"
            )));
        }
        db::profiles::share(conn, namespace_id, with_user_id, role.as_str())
            .map_err(NamespaceError::Other)?;
        Ok(())
    }

    /// Remove a share. Requires admin (owner).
    pub fn unshare(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
        with_user_id: &str,
    ) -> Result<bool, NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_admin() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        db::profiles::unshare(conn, namespace_id, with_user_id).map_err(NamespaceError::Other)
    }

    /// List sharees and their roles. Requires admin (owner).
    pub fn list_shares(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
    ) -> Result<Vec<(String, Role)>, NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_admin() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        let rows = db::profiles::list_shares(conn, namespace_id).map_err(NamespaceError::Other)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let role = Role::parse(&row.role)
                .ok_or_else(|| NamespaceError::InvalidRole(row.role.clone()))?;
            out.push((row.user_id, role));
        }
        Ok(out)
    }

    /// Set the active profile for a user. Caller must already have read access.
    pub fn set_active(
        &self,
        conn: &Connection,
        user_id: &str,
        namespace_id: &str,
    ) -> Result<(), NamespaceError> {
        let access = self.access(conn, namespace_id, user_id)?;
        if !access.can_read() {
            return Err(NamespaceError::PermissionDenied {
                user: user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        db::profiles::set_active(conn, user_id, namespace_id).map_err(NamespaceError::Other)?;
        Ok(())
    }

    /// Resolve a user's active profile, falling back to ORCA_PROFILE env, then
    /// the user's first owned profile, then None.
    pub fn resolve_active(
        &self,
        conn: &Connection,
        user_id: &str,
    ) -> Result<Option<Namespace>, NamespaceError> {
        // 1. ORCA_PROFILE env (id or name)
        if let Ok(spec) = std::env::var("ORCA_PROFILE")
            && let Some(p) = self.resolve_spec(conn, user_id, &spec)?
        {
            return Ok(Some(p));
        }
        // 2. Persisted active selection
        if let Some(id) = db::profiles::get_active(conn, user_id).map_err(NamespaceError::Other)? {
            // ACL-check; if access lapsed, fall through.
            if let Ok(p) = self.get(conn, &id, user_id) {
                return Ok(Some(p));
            }
        }
        // 3. First accessible profile
        let mut accessible = self.list_for_user(conn, user_id)?;
        Ok(accessible.drain(..).next())
    }

    /// Resolve a profile by id-or-name spec. Tries id first, then `name` owned
    /// by the requesting user. Returns None if nothing matches.
    pub fn resolve_spec(
        &self,
        conn: &Connection,
        requesting_user_id: &str,
        spec: &str,
    ) -> Result<Option<Namespace>, NamespaceError> {
        // Try as id (with ACL check)
        if let Ok(p) = self.get(conn, spec, requesting_user_id) {
            return Ok(Some(p));
        }
        // Try as name owned by requesting user
        self.get_by_owner_and_name(conn, requesting_user_id, spec)
    }

    /// First-run bootstrap: ensure `owner_user_id` has at least one profile.
    /// Creates a `default` profile if none exists. Returns the active or
    /// newly-created profile.
    pub fn ensure_default_for(
        &self,
        conn: &Connection,
        owner_user_id: &str,
    ) -> Result<Namespace, NamespaceError> {
        if let Some(active) = self.resolve_active(conn, owner_user_id)? {
            return Ok(active);
        }
        let p = self.create(
            conn,
            owner_user_id,
            "default",
            Some("Default profile created on first run"),
        )?;
        db::profiles::set_active(conn, owner_user_id, &p.id).map_err(NamespaceError::Other)?;
        Ok(p)
    }

    /// Set a credential on a profile. Stored in the encrypted DB (Tier 2).
    /// Requires write access.
    pub fn set_credential(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_write() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        db::profile_creds::set(conn, namespace_id, key, value).map_err(NamespaceError::Other)?;
        Ok(())
    }

    /// Read a credential. Requires read access.
    pub fn get_credential(
        &self,
        conn: &Connection,
        namespace_id: &str,
        requesting_user_id: &str,
        key: &str,
    ) -> Result<Option<String>, NamespaceError> {
        let access = self.access(conn, namespace_id, requesting_user_id)?;
        if !access.can_read() {
            return Err(NamespaceError::PermissionDenied {
                user: requesting_user_id.to_string(),
                namespace: namespace_id.to_string(),
            });
        }
        db::profile_creds::get(conn, namespace_id, key).map_err(NamespaceError::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a fresh DB with full schema applied. Uses an unencrypted on-disk
    /// file in a tempdir because `db::open_unencrypted` is the public entry
    /// point that runs both `apply_schema` and pending migrations.
    fn test_conn() -> (Connection, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let conn = db::open_unencrypted(&path).expect("open_unencrypted");
        (conn, tmp)
    }

    fn manager() -> (NamespaceManager, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        (NamespaceManager::new(tmp.path().join("namespaces")), tmp)
    }

    #[test]
    fn create_and_get_profile() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr
            .create(&conn, "alice", "homelab", Some("home stuff"))
            .unwrap();
        assert_eq!(p.name, "homelab");
        assert_eq!(p.owner_user_id, "alice");
        assert!(p.agents_dir().exists());
        assert!(p.contexts_dir().exists());

        let fetched = mgr.get(&conn, &p.id, "alice").unwrap();
        assert_eq!(fetched.id, p.id);
    }

    #[test]
    fn create_rejects_duplicate_name_per_owner() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        mgr.create(&conn, "alice", "homelab", None).unwrap();
        let err = mgr.create(&conn, "alice", "homelab", None).unwrap_err();
        assert!(matches!(err, NamespaceError::NameTaken(_)));
    }

    #[test]
    fn different_owners_can_have_same_name() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        mgr.create(&conn, "alice", "homelab", None).unwrap();
        mgr.create(&conn, "bob", "homelab", None).unwrap();
    }

    #[test]
    fn get_denies_non_sharee() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();
        let err = mgr.get(&conn, &p.id, "bob").unwrap_err();
        assert!(matches!(err, NamespaceError::PermissionDenied { .. }));
    }

    #[test]
    fn share_grants_read_to_viewer_and_write_to_collaborator() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();

        mgr.share(&conn, &p.id, "alice", "bob", Role::Viewer)
            .unwrap();
        assert_eq!(mgr.access(&conn, &p.id, "bob").unwrap(), Access::Viewer);
        assert!(mgr.access(&conn, &p.id, "bob").unwrap().can_read());
        assert!(!mgr.access(&conn, &p.id, "bob").unwrap().can_write());

        mgr.share(&conn, &p.id, "alice", "carol", Role::Collaborator)
            .unwrap();
        assert_eq!(
            mgr.access(&conn, &p.id, "carol").unwrap(),
            Access::Collaborator
        );
        assert!(mgr.access(&conn, &p.id, "carol").unwrap().can_write());
        assert!(!mgr.access(&conn, &p.id, "carol").unwrap().can_admin());
    }

    #[test]
    fn list_for_user_includes_owned_and_shared() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let alice_p = mgr.create(&conn, "alice", "homelab", None).unwrap();
        let _bob_p = mgr.create(&conn, "bob", "work", None).unwrap();
        mgr.share(&conn, &alice_p.id, "alice", "bob", Role::Viewer)
            .unwrap();

        let bob_list = mgr.list_for_user(&conn, "bob").unwrap();
        let names: Vec<_> = bob_list.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains(&"work".to_string()));
        assert!(names.contains(&"homelab".to_string()));
        assert_eq!(bob_list.len(), 2);
    }

    #[test]
    fn collaborator_cannot_share_or_delete() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();
        mgr.share(&conn, &p.id, "alice", "bob", Role::Collaborator)
            .unwrap();

        assert!(matches!(
            mgr.share(&conn, &p.id, "bob", "carol", Role::Viewer)
                .unwrap_err(),
            NamespaceError::PermissionDenied { .. }
        ));
        assert!(matches!(
            mgr.delete(&conn, &p.id, "bob").unwrap_err(),
            NamespaceError::PermissionDenied { .. }
        ));
    }

    #[test]
    fn ensure_default_creates_then_returns_existing() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p1 = mgr.ensure_default_for(&conn, "alice").unwrap();
        assert_eq!(p1.name, "default");
        let p2 = mgr.ensure_default_for(&conn, "alice").unwrap();
        assert_eq!(p1.id, p2.id);
    }

    #[test]
    fn resolve_spec_finds_by_name_then_id() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();

        let by_name = mgr
            .resolve_spec(&conn, "alice", "homelab")
            .unwrap()
            .unwrap();
        assert_eq!(by_name.id, p.id);

        let by_id = mgr.resolve_spec(&conn, "alice", &p.id).unwrap().unwrap();
        assert_eq!(by_id.id, p.id);

        assert!(mgr.resolve_spec(&conn, "alice", "nope").unwrap().is_none());
    }

    #[test]
    fn delete_removes_filesystem_content() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();
        let agents = p.agents_dir();
        std::fs::write(agents.join("notes.md"), "hello").unwrap();
        mgr.delete(&conn, &p.id, "alice").unwrap();
        assert!(!p.root.exists());
    }

    #[test]
    fn credentials_require_write_access() {
        let (conn, _td_db) = test_conn();
        let (mgr, _td) = manager();
        let p = mgr.create(&conn, "alice", "homelab", None).unwrap();
        mgr.share(&conn, &p.id, "alice", "bob", Role::Viewer)
            .unwrap();

        // Owner can write
        mgr.set_credential(&conn, &p.id, "alice", "api_token", "abc")
            .unwrap();
        assert_eq!(
            mgr.get_credential(&conn, &p.id, "alice", "api_token")
                .unwrap(),
            Some("abc".to_string())
        );

        // Viewer can read
        assert_eq!(
            mgr.get_credential(&conn, &p.id, "bob", "api_token")
                .unwrap(),
            Some("abc".to_string())
        );

        // Viewer cannot write
        assert!(matches!(
            mgr.set_credential(&conn, &p.id, "bob", "api_token", "xyz")
                .unwrap_err(),
            NamespaceError::PermissionDenied { .. }
        ));
    }
}
