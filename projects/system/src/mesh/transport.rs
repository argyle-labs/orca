// Bundles are heterogeneous per-entity JSON — same allow rationale as
// db::replicate_engine and replicate_wire.
#![allow(clippy::disallowed_types)]

//! Mesh-mesh implementation of `db::replicate_engine::ReplicationTransport`.
//!
//! Engine in `db` decides what + when; this transport implements the wire
//! shape: bootstrap-key signing, mTLS dial, pinned-fp verification. Register
//! at daemon boot before the engine spawns.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use db::replicate_engine::{ReplicationTransport, TransportPeer};
use serde_json::Value;

use crate::mesh::{
    ReplicateBundle, fetch_replicate_bundle, fetch_replicate_roots, pki_dir, push_replicate_bundle,
};
use db::mesh as pdb;

pub struct MeshTransport;

impl MeshTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait]
impl ReplicationTransport for MeshTransport {
    async fn list_peers(&self) -> Result<Vec<TransportPeer>> {
        let own_peer_id = crate::host_identity::machine_id().to_string();
        let rows = db::pool::with_pooled_or_open(pdb::list_peers)?;
        Ok(rows
            .into_iter()
            .filter(|p| {
                p.departed_at.is_none() && p.peer_id != "unknown" && p.peer_id != own_peer_id
            })
            .map(|p| TransportPeer {
                peer_id: p.peer_id,
                hostname: p.peer_hostname,
                addr: p.peer_addr,
                pinned_fp: p.pubkey_fp,
            })
            .collect())
    }

    async fn push(&self, peer: &TransportPeer, bundle: &BTreeMap<String, Value>) -> Result<usize> {
        let envelope = sign_bundle(
            bundle.clone(),
            crate::mesh::MESH_REPLICATE_PUSH_METHOD,
            &peer.peer_id,
            &utils::id::new(),
        )?;
        let targets = dial_targets(peer);
        crate::mesh::dialer::try_targets_tracked(Some(&peer.peer_id), &targets, |t| {
            let envelope = envelope.clone();
            async move { push_replicate_bundle(&t, &envelope).await }
        })
        .await
    }

    async fn fetch(&self, peer: &TransportPeer) -> Result<BTreeMap<String, Value>> {
        let pinned_fp = peer
            .pinned_fp
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("peer {} has no pinned fp", peer.hostname))?;
        let targets = dial_targets(peer);
        let nonce = utils::id::new();
        let envelope =
            crate::mesh::dialer::try_targets_tracked(Some(&peer.peer_id), &targets, |t| {
                let nonce = nonce.clone();
                async move { fetch_replicate_bundle(&t, &nonce).await }
            })
            .await?;
        verify_export(
            &envelope,
            pinned_fp,
            &peer.peer_id,
            crate::host_identity::machine_id(),
            &nonce,
            utils::time::now().unix_seconds(),
        )
    }

    async fn fetch_roots(&self, peer: &TransportPeer) -> Result<BTreeMap<String, String>> {
        let targets = dial_targets(peer);
        let r = crate::mesh::dialer::try_targets_tracked(
            Some(&peer.peer_id),
            &targets,
            |t| async move { fetch_replicate_roots(&t).await },
        )
        .await?;
        Ok(r.roots)
    }
}

/// Ordered dial targets for a replication peer: every known address (LAN v4/v6,
/// Tailscale, fqdn) with the legacy `peer_addr` last, so replication reaches a
/// dual-homed peer even when its primary interface is momentarily down. Falls
/// back to the single legacy addr if the DB is unreachable.
fn dial_targets(peer: &TransportPeer) -> Vec<String> {
    db::pool::with_pooled_or_open(|conn| {
        Ok(
            crate::mesh::dialer::dial_targets_for_peer(conn, &peer.peer_id, &peer.addr)
                .unwrap_or_else(|_| vec![peer.addr.clone()]),
        )
    })
    .unwrap_or_else(|_: anyhow::Error| vec![peer.addr.clone()])
}

pub const BUNDLE_TTL_SECS: i64 = 120;

/// Sign `entities` with this host's bootstrap key, bound to `method`,
/// `receiver` and `nonce`. Shared by push (transport) and the
/// `mesh/replicate-export` handler.
pub fn sign_bundle(
    entities: BTreeMap<String, Value>,
    method: &str,
    receiver: &str,
    nonce: &str,
) -> Result<utils::pki::SignedEnvelope> {
    let sender = crate::host_identity::machine_id().to_string();
    let now = utils::time::now().unix_seconds();
    let body = ReplicateBundle {
        peer_id: sender.clone(),
        issued_at: now,
        entities,
    };
    let signing = utils::pki::load_or_init_bootstrap_key(&pki_dir())?;
    utils::pki::sign_bound_envelope(
        &signing,
        &body,
        method,
        &sender,
        receiver,
        nonce,
        now,
        BUNDLE_TTL_SECS,
    )
    .context("sign replicate bundle")
}

/// Verify a bound bundle from `sender` to `receiver` against `sender`'s
/// pinned bootstrap fp. Returns the entities and the binding, whose nonce the
/// caller must record or match.
pub fn verify_bundle<'a>(
    envelope: &'a utils::pki::SignedEnvelope,
    pinned_fp: &str,
    method: &str,
    sender: &str,
    receiver: &str,
    now: i64,
) -> Result<(BTreeMap<String, Value>, &'a utils::pki::BoundEnvelope)> {
    let (bundle, verifying, binding) = utils::pki::verify_bound_envelope::<ReplicateBundle>(
        envelope, method, sender, receiver, now,
    )
    .context("verify bundle envelope")?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&verifying);
    anyhow::ensure!(
        signer_fp == pinned_fp,
        "replicate bundle signer fp {signer_fp} does not match pinned peer fp {pinned_fp}"
    );
    Ok((bundle.entities, binding))
}

/// Verify an exported bundle answers this host's own request `nonce`.
pub fn verify_export(
    envelope: &utils::pki::SignedEnvelope,
    pinned_fp: &str,
    sender: &str,
    receiver: &str,
    nonce: &str,
    now: i64,
) -> Result<BTreeMap<String, Value>> {
    let (entities, binding) = verify_bundle(
        envelope,
        pinned_fp,
        crate::mesh::MESH_REPLICATE_EXPORT_METHOD,
        sender,
        receiver,
        now,
    )?;
    anyhow::ensure!(
        binding.nonce == nonce,
        "replicate export answers another request nonce"
    );
    Ok(entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_entities() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert("users".into(), Value::Array(vec![]));
        m
    }

    fn bound(signing: &ed25519_dalek::SigningKey) -> utils::pki::SignedEnvelope {
        let body = ReplicateBundle {
            peer_id: "a".into(),
            issued_at: 0,
            entities: empty_entities(),
        };
        utils::pki::sign_bound_envelope(
            signing,
            &body,
            crate::mesh::MESH_REPLICATE_PUSH_METHOD,
            "a",
            "b",
            "019f0000-0000-7000-8000-000000000001",
            100,
            60,
        )
        .unwrap()
    }

    #[test]
    fn sign_then_verify_bundle_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let signing = utils::pki::load_or_init_bootstrap_key(tmp.path()).unwrap();
        let fp = utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());
        let envelope = bound(&signing);
        let (entities, _) = verify_bundle(
            &envelope,
            &fp,
            crate::mesh::MESH_REPLICATE_PUSH_METHOD,
            "a",
            "b",
            110,
        )
        .unwrap();
        assert!(entities.contains_key("users"));
    }

    #[test]
    fn verify_bundle_rejects_wrong_fp() {
        let tmp = tempfile::tempdir().unwrap();
        let signing = utils::pki::load_or_init_bootstrap_key(tmp.path()).unwrap();
        let err = verify_bundle(
            &bound(&signing),
            "not-the-actual-fp",
            crate::mesh::MESH_REPLICATE_PUSH_METHOD,
            "a",
            "b",
            110,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("does not match pinned"),
            "expected pinned-fp mismatch, got: {err}"
        );
    }
}
