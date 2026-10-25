//! CA-signed, replicated records: the secure-member registry and mesh policy
//! switches. Rows are append-only and verified against the mesh CA both when
//! a peer's row is merged and when a row is read, so a forged row can neither
//! land nor erase a record.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub const SECURE_GRANTED: &str = "granted";
pub const SECURE_REVOKED: &str = "revoked";
pub const POLICY_REQUIRE_BOUND: &str = "require_bound_envelopes";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, derive::Replicated)]
#[replicate(
    crate = ::macro_runtime,
    table = "mesh_secure_registry",
    pk = "id",
    lww = "updated_at",
    immutable = "true",
    verify = verify_registry_row
)]
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
#[replicate(
    crate = ::macro_runtime,
    table = "mesh_policy",
    pk = "id",
    lww = "updated_at",
    immutable = "true",
    verify = verify_policy_row
)]
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

/// A mesh CA this host trusts: the current CA (rank 1) or the previous one
/// still inside its rotation overlap (rank 0).
pub struct MeshCa {
    pub fp: String,
    pub cert: String,
    pub rank: u8,
}

pub fn ca_fp(cert_pem: &str) -> String {
    utils::hash::sha256_hex(cert_pem.trim().as_bytes())
}

pub fn known_cas(pki: &Path) -> Vec<MeshCa> {
    [
        (utils::pki::mesh_ca_previous_cert_path(pki), 0),
        (utils::pki::mesh_ca_cert_path(pki), 1),
    ]
    .into_iter()
    .filter_map(|(path, rank)| {
        let cert = std::fs::read_to_string(path).ok()?;
        Some(MeshCa {
            fp: ca_fp(&cert),
            cert,
            rank,
        })
    })
    .collect()
}

/// Rank of the trusted CA named by `ca_fp` when it signed `fields`.
pub fn verified_rank(
    cas: &[MeshCa],
    ca_fp: &str,
    fields: &[String],
    signature: &str,
) -> Option<u8> {
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    cas.iter()
        .find(|ca| ca.fp == ca_fp)
        .filter(|ca| utils::pki::verify_ca_record(&ca.cert, &fields, signature).is_ok())
        .map(|ca| ca.rank)
}

fn signed_by_this_mesh(ca_fp: &str, fields: &[String], signature: &str) -> bool {
    contract::config::paths::pki_dir()
        .is_ok_and(|pki| verified_rank(&known_cas(&pki), ca_fp, fields, signature).is_some())
}

fn verify_registry_row(_: &Connection, row: &SecureRegistryEntry) -> bool {
    signed_by_this_mesh(&row.ca_fp, &row.signed_fields(), &row.signature)
}

fn verify_policy_row(_: &Connection, row: &MeshPolicyEntry) -> bool {
    signed_by_this_mesh(&row.ca_fp, &row.signed_fields(), &row.signature)
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
