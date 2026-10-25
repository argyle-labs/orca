//! The CA-signed secure-member registry and mesh policy switches on top of
//! `db::mesh::registry`. Only a mesh-CA key holder can write a record; every
//! reader verifies it against the current or previous CA cert.
//!
//! A member may receive CA key material when a verified grant names it and no
//! verified revocation was issued under the same or a newer CA. A revoked
//! member can sign under the CA it kept, but never under the CA that rotation
//! withholds from it, so only a rotation can lift a revocation.

use anyhow::{Context, Result};
use db::mesh as pdb;
use rusqlite::Connection;
use std::path::Path;

struct Ca {
    fp: String,
    cert: String,
    rank: u8,
}

fn ca_fp(cert_pem: &str) -> String {
    utils::hash::sha256_hex(cert_pem.trim().as_bytes())
}

fn known_cas(pki: &Path) -> Vec<Ca> {
    [
        (utils::pki::mesh_ca_previous_cert_path(pki), 0),
        (utils::pki::mesh_ca_cert_path(pki), 1),
    ]
    .into_iter()
    .filter_map(|(path, rank)| {
        let cert = std::fs::read_to_string(path).ok()?;
        Some(Ca {
            fp: ca_fp(&cert),
            cert,
            rank,
        })
    })
    .collect()
}

fn verified_rank(cas: &[Ca], ca_fp: &str, fields: &[String], signature: &str) -> Option<u8> {
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    cas.iter()
        .find(|ca| ca.fp == ca_fp)
        .filter(|ca| utils::pki::verify_ca_record(&ca.cert, &fields, signature).is_ok())
        .map(|ca| ca.rank)
}

struct Signer {
    key: String,
    fp: String,
}

fn signer(pki: &Path) -> Result<Signer> {
    let key = std::fs::read_to_string(utils::pki::mesh_ca_key_path(pki))
        .context("only a member holding the mesh CA key can sign registry records")?;
    let cert = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(pki))?;
    Ok(Signer {
        key,
        fp: ca_fp(&cert),
    })
}

fn sign(signer: &Signer, fields: &[String]) -> Result<String> {
    let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
    utils::pki::sign_ca_record(&signer.key, &fields)
}

fn record(conn: &Connection, signer: &Signer, member_id: &str, state: &str) -> Result<()> {
    let mut entry = pdb::SecureRegistryEntry {
        id: utils::id::new(),
        member_id: member_id.to_string(),
        state: state.to_string(),
        ca_fp: signer.fp.clone(),
        updated_at: utils::time::now().to_rfc3339(),
        signature: String::new(),
    };
    entry.signature = sign(signer, &entry.signed_fields())?;
    pdb::insert_registry_entry(conn, &entry)
}

fn ranks(conn: &Connection, pki: &Path, member_id: &str, state: &str) -> Vec<u8> {
    let cas = known_cas(pki);
    pdb::registry_entries(conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.member_id == member_id && e.state == state)
        .filter_map(|e| verified_rank(&cas, &e.ca_fp, &e.signed_fields(), &e.signature))
        .collect()
}

pub(crate) fn is_revoked(conn: &Connection, pki: &Path, member_id: &str) -> bool {
    let newest_grant = ranks(conn, pki, member_id, pdb::SECURE_GRANTED)
        .into_iter()
        .max();
    ranks(conn, pki, member_id, pdb::SECURE_REVOKED)
        .into_iter()
        .any(|r| newest_grant.is_none_or(|g| r >= g))
}

pub(crate) fn is_registered_secure(conn: &Connection, pki: &Path, member_id: &str) -> bool {
    !ranks(conn, pki, member_id, pdb::SECURE_GRANTED).is_empty()
        && !is_revoked(conn, pki, member_id)
}

pub(crate) fn ensure_grantable(conn: &Connection, pki: &Path, member_id: &str) -> Result<()> {
    anyhow::ensure!(
        !ranks(conn, pki, member_id, pdb::SECURE_REVOKED)
            .into_iter()
            .any(|r| r == 1),
        "{member_id} was revoked under the current mesh CA; rotate the CA \
         (`orca mesh ca rotate`) before promoting it again"
    );
    Ok(())
}

pub(crate) fn grant(conn: &Connection, pki: &Path, member_id: &str) -> Result<()> {
    let signer = signer(pki)?;
    ensure_grantable(conn, pki, member_id)?;
    record(conn, &signer, member_id, pdb::SECURE_GRANTED)
}

pub(crate) fn revoke(conn: &Connection, pki: &Path, member_id: &str) -> Result<()> {
    record(conn, &signer(pki)?, member_id, pdb::SECURE_REVOKED)
}

/// Register this member when it is a secure CA holder the registry does not
/// list yet (founders, and holders that predate the registry).
pub(crate) fn ensure_self_registered(conn: &Connection, pki: &Path, own_id: &str) -> Result<()> {
    if pdb::get_secure(conn)?
        && utils::pki::has_mesh_ca_key(pki)
        && !is_registered_secure(conn, pki, own_id)
        && !is_revoked(conn, pki, own_id)
    {
        grant(conn, pki, own_id)?;
    }
    Ok(())
}

/// Demote this member locally when the registry revokes it, so the CA key it
/// holds is purged without its cooperation.
pub(crate) fn apply_own_revocation(conn: &Connection, pki: &Path, own_id: &str) -> Result<bool> {
    if is_revoked(conn, pki, own_id) && pdb::get_secure(conn)? {
        pdb::set_secure(conn, false)?;
        return Ok(true);
    }
    Ok(false)
}

/// Rotation and key pushes go only to a member the registry grants that has
/// also just proven its claim (`proved`).
pub(crate) fn may_receive_ca_material(
    conn: &Connection,
    pki: &Path,
    member_id: &str,
    proved: bool,
) -> bool {
    proved && is_registered_secure(conn, pki, member_id)
}

/// Re-sign, under the current CA, every grant that is still in force and every
/// policy switch, so records outlive the previous CA's overlap window.
/// Revocations are not carried: a revoked member never receives the new CA,
/// so it cannot forge a grant under it.
pub(crate) fn resign_all(conn: &Connection, pki: &Path) -> Result<()> {
    let signer = signer(pki)?;
    let mut members: Vec<String> = pdb::registry_entries(conn)?
        .into_iter()
        .map(|e| e.member_id)
        .collect();
    members.sort();
    members.dedup();
    for m in members {
        if is_registered_secure(conn, pki, &m) {
            record(conn, &signer, &m, pdb::SECURE_GRANTED)?;
        }
    }
    if require_bound(conn, pki) {
        record_require_bound(conn, &signer)?;
    }
    Ok(())
}

pub(crate) fn require_bound(conn: &Connection, pki: &Path) -> bool {
    let cas = known_cas(pki);
    pdb::policy_entries(conn)
        .unwrap_or_default()
        .iter()
        .any(|e| {
            e.key == pdb::POLICY_REQUIRE_BOUND
                && e.value == "true"
                && verified_rank(&cas, &e.ca_fp, &e.signed_fields(), &e.signature).is_some()
        })
}

fn record_require_bound(conn: &Connection, signer: &Signer) -> Result<()> {
    let mut entry = pdb::MeshPolicyEntry {
        id: utils::id::new(),
        key: pdb::POLICY_REQUIRE_BOUND.to_string(),
        value: "true".to_string(),
        ca_fp: signer.fp.clone(),
        updated_at: utils::time::now().to_rfc3339(),
        signature: String::new(),
    };
    entry.signature = sign(signer, &entry.signed_fields())?;
    pdb::insert_policy_entry(conn, &entry)
}

/// Turn on the mesh-wide refusal of unbound envelopes. One-way. Refused until
/// every active member has advertised bound-envelope support.
pub(crate) fn enable_require_bound(conn: &Connection, pki: &Path) -> Result<()> {
    let laggards = pdb::members_without_bound_envelopes(conn)?;
    anyhow::ensure!(
        laggards.is_empty(),
        "require-bound refused: these members have not advertised bound-envelope support: {}",
        laggards.join(", ")
    );
    record_require_bound(conn, &signer(pki)?)
}

/// An unbound envelope from `peer_id` is accepted only while the mesh has not
/// switched to require-bound and the peer never advertised bound support.
pub(crate) fn unbound_allowed(conn: &Connection, pki: &Path, peer_id: &str) -> bool {
    !require_bound(conn, pki) && !pdb::advertises_bound_envelopes(conn, peer_id).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Connection, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let pki = dir.path().join("pki");
        std::fs::create_dir_all(&pki).unwrap();
        utils::pki::init_mesh_ca(&pki, "holder").unwrap();
        let conn = db::open_unencrypted(&dir.path().join("orca.db")).unwrap();
        (dir, conn, pki)
    }

    #[test]
    fn a_demoted_holder_that_kept_its_key_is_excluded() {
        let (_d, c, pki) = fixture();
        grant(&c, &pki, "m1").unwrap();
        assert!(may_receive_ca_material(&c, &pki, "m1", true));
        revoke(&c, &pki, "m1").unwrap();
        assert!(!may_receive_ca_material(&c, &pki, "m1", true));
        assert!(is_revoked(&c, &pki, "m1"));
    }

    #[test]
    fn a_revoked_holder_cannot_re_grant_itself_with_the_key_it_kept() {
        let (_d, c, pki) = fixture();
        grant(&c, &pki, "m1").unwrap();
        revoke(&c, &pki, "m1").unwrap();
        record(&c, &signer(&pki).unwrap(), "m1", pdb::SECURE_GRANTED).unwrap();
        assert!(!is_registered_secure(&c, &pki, "m1"));
        let err = grant(&c, &pki, "m1").unwrap_err();
        assert!(err.to_string().contains("rotate"), "{err}");
    }

    #[test]
    fn rotation_lifts_a_revocation_only_for_a_new_grant() {
        let (_d, c, pki) = fixture();
        grant(&c, &pki, "m1").unwrap();
        grant(&c, &pki, "m2").unwrap();
        revoke(&c, &pki, "m1").unwrap();
        utils::pki::rotate_mesh_ca(&pki).unwrap();
        resign_all(&c, &pki).unwrap();
        utils::pki::drop_mesh_ca_previous(&pki).unwrap();
        assert!(is_registered_secure(&c, &pki, "m2"));
        assert!(!is_registered_secure(&c, &pki, "m1"));
        grant(&c, &pki, "m1").unwrap();
        assert!(is_registered_secure(&c, &pki, "m1"));
    }

    #[test]
    fn a_revoked_member_demotes_itself_and_a_holder_self_registers() {
        let (_d, c, pki) = fixture();
        pdb::set_secure(&c, true).unwrap();
        ensure_self_registered(&c, &pki, "me").unwrap();
        assert!(is_registered_secure(&c, &pki, "me"));
        assert!(!apply_own_revocation(&c, &pki, "me").unwrap());
        revoke(&c, &pki, "me").unwrap();
        assert!(apply_own_revocation(&c, &pki, "me").unwrap());
        assert!(!pdb::get_secure(&c).unwrap());
    }

    #[test]
    fn an_unregistered_or_unproven_member_is_excluded() {
        let (_d, c, pki) = fixture();
        assert!(!may_receive_ca_material(&c, &pki, "m1", true));
        grant(&c, &pki, "m1").unwrap();
        assert!(!may_receive_ca_material(&c, &pki, "m1", false));
    }

    #[test]
    fn a_registry_record_not_signed_by_the_mesh_ca_is_ignored() {
        let (_d, c, pki) = fixture();
        let other = tempfile::tempdir().unwrap();
        utils::pki::init_mesh_ca(other.path(), "rogue").unwrap();
        grant(&c, other.path(), "m1").unwrap();
        assert!(!is_registered_secure(&c, &pki, "m1"));
    }

    #[test]
    fn require_bound_needs_every_member_and_a_ca_signature() {
        let (_d, c, pki) = fixture();
        let id = "019f0000-0000-7000-8000-000000000001";
        pdb::upsert_peer(&c, id, "a", "10.0.0.1", 1, None, "").unwrap();
        assert!(unbound_allowed(&c, &pki, id));
        let err = enable_require_bound(&c, &pki).unwrap_err();
        assert!(err.to_string().contains(id), "{err}");
        pdb::mark_bound_envelopes(&c, id).unwrap();
        assert!(!unbound_allowed(&c, &pki, id));
        enable_require_bound(&c, &pki).unwrap();
        assert!(require_bound(&c, &pki));
        let other = tempfile::tempdir().unwrap();
        utils::pki::init_mesh_ca(other.path(), "rogue").unwrap();
        assert!(!require_bound(&c, other.path()));
        utils::pki::rotate_mesh_ca(&pki).unwrap();
        resign_all(&c, &pki).unwrap();
        utils::pki::drop_mesh_ca_previous(&pki).unwrap();
        assert!(require_bound(&c, &pki));
    }
}
