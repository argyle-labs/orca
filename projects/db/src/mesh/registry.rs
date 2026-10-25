//! CA-signed, replicated records: the secure-member registry and mesh policy
//! switches. Rows are append-only: a peer's copy of an existing row can only
//! advance `updated_at`, never its signed fields, and every reader verifies
//! the mesh-CA signature, so a forged row cannot create or erase a record.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub const SECURE_GRANTED: &str = "granted";
pub const SECURE_REVOKED: &str = "revoked";
pub const POLICY_REQUIRE_BOUND: &str = "require_bound_envelopes";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, derive::Replicated)]
#[replicate(crate = ::macro_runtime, table = "mesh_secure_registry", pk = "id", lww = "updated_at")]
pub struct SecureRegistryEntry {
    pub id: String,
    pub member_id: String,
    pub state: String,
    pub ca_fp: String,
    pub updated_at: String,
    pub signature: String,
}

impl SecureRegistryEntry {
    pub fn signed_fields(&self) -> [String; 5] {
        [
            "orca-secure-registry-v1".into(),
            self.id.clone(),
            self.member_id.clone(),
            self.state.clone(),
            self.ca_fp.clone(),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, derive::Replicated)]
#[replicate(crate = ::macro_runtime, table = "mesh_policy", pk = "id", lww = "updated_at")]
pub struct MeshPolicyEntry {
    pub id: String,
    pub key: String,
    pub value: String,
    pub ca_fp: String,
    pub updated_at: String,
    pub signature: String,
}

impl MeshPolicyEntry {
    pub fn signed_fields(&self) -> [String; 5] {
        [
            "orca-mesh-policy-v1".into(),
            self.id.clone(),
            self.key.clone(),
            self.value.clone(),
            self.ca_fp.clone(),
        ]
    }
}

pub fn insert_registry_entry(conn: &Connection, e: &SecureRegistryEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO mesh_secure_registry (id, member_id, state, ca_fp, updated_at, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            e.id,
            e.member_id,
            e.state,
            e.ca_fp,
            e.updated_at,
            e.signature
        ],
    )?;
    Ok(())
}

pub fn registry_entries(conn: &Connection) -> Result<Vec<SecureRegistryEntry>> {
    conn.prepare(
        "SELECT id, member_id, state, ca_fp, updated_at, signature FROM mesh_secure_registry",
    )?
    .query_map([], |r| {
        Ok(SecureRegistryEntry {
            id: r.get(0)?,
            member_id: r.get(1)?,
            state: r.get(2)?,
            ca_fp: r.get(3)?,
            updated_at: r.get(4)?,
            signature: r.get(5)?,
        })
    })?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

pub fn insert_policy_entry(conn: &Connection, e: &MeshPolicyEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO mesh_policy (id, key, value, ca_fp, updated_at, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![e.id, e.key, e.value, e.ca_fp, e.updated_at, e.signature],
    )?;
    Ok(())
}

pub fn policy_entries(conn: &Connection) -> Result<Vec<MeshPolicyEntry>> {
    conn.prepare("SELECT id, key, value, ca_fp, updated_at, signature FROM mesh_policy")?
        .query_map([], |r| {
            Ok(MeshPolicyEntry {
                id: r.get(0)?,
                key: r.get(1)?,
                value: r.get(2)?,
                ca_fp: r.get(3)?,
                updated_at: r.get(4)?,
                signature: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

/// Record that `peer_id` advertised bound-envelope support. Never cleared, so
/// a member that once advertised support can never be downgraded to unbound.
pub fn mark_bound_envelopes(conn: &Connection, peer_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE mesh_peers SET bound_envelopes = 1 WHERE peer_id = ?1",
        params![peer_id],
    )?;
    Ok(())
}

pub fn advertises_bound_envelopes(conn: &Connection, peer_id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT bound_envelopes FROM mesh_peers WHERE peer_id = ?1",
            params![peer_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|v| v != 0))
}

/// Active members that have not advertised bound-envelope support.
pub fn members_without_bound_envelopes(conn: &Connection) -> Result<Vec<String>> {
    conn.prepare(
        "SELECT peer_id FROM mesh_peers WHERE departed_at IS NULL AND bound_envelopes = 0",
    )?
    .query_map([], |r| r.get(0))?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let c = crate::open_unencrypted(&dir.path().join("orca.db")).unwrap();
        (dir, c)
    }

    fn entry(member: &str) -> SecureRegistryEntry {
        SecureRegistryEntry {
            id: utils::id::new(),
            member_id: member.into(),
            state: SECURE_GRANTED.into(),
            ca_fp: "fp".into(),
            updated_at: "2026-10-25T00:00:00.000Z".into(),
            signature: "sig".into(),
        }
    }

    #[test]
    fn a_replicated_registry_row_keeps_its_signed_fields() {
        let (_d, c) = conn();
        let original = entry("m1");
        insert_registry_entry(&c, &original).unwrap();
        let mut forged = original.clone();
        forged.state = SECURE_REVOKED.into();
        forged.signature = "forged".into();
        forged.updated_at = "2026-10-25T00:00:09.000Z".into();
        let rows = serde_json::to_value(vec![forged]).unwrap();
        macro_runtime::replicate_table::merge_table(
            &c,
            "mesh_secure_registry",
            &[
                "id",
                "member_id",
                "state",
                "ca_fp",
                "updated_at",
                "signature",
            ],
            "id",
            "updated_at",
            &[],
            rows,
        )
        .unwrap();
        let kept = registry_entries(&c).unwrap().remove(0);
        assert_eq!(
            (kept.state, kept.signature),
            (original.state, original.signature)
        );
    }

    #[test]
    fn bound_envelope_support_is_sticky_and_listed() {
        let (_d, c) = conn();
        crate::mesh::upsert_peer(
            &c,
            "019f0000-0000-7000-8000-000000000001",
            "a",
            "10.0.0.1",
            1,
            None,
            "",
        )
        .unwrap();
        crate::mesh::upsert_peer(
            &c,
            "019f0000-0000-7000-8000-000000000002",
            "b",
            "10.0.0.2",
            1,
            None,
            "",
        )
        .unwrap();
        mark_bound_envelopes(&c, "019f0000-0000-7000-8000-000000000001").unwrap();
        assert!(advertises_bound_envelopes(&c, "019f0000-0000-7000-8000-000000000001").unwrap());
        assert_eq!(
            members_without_bound_envelopes(&c).unwrap(),
            vec!["019f0000-0000-7000-8000-000000000002".to_string()]
        );
    }
}
