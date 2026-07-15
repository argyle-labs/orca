// Bundles are heterogeneous per-entity JSON — same allow rationale as
// db::replicate_engine and replicate_wire.
#![allow(clippy::disallowed_types)]

//! Pod-mesh implementation of `db::replicate_engine::ReplicationTransport`.
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

use crate::{
    ReplicateBundle, fetch_replicate_bundle, fetch_replicate_roots, pki_dir, push_replicate_bundle,
};
use db::pod as pdb;

pub struct PodMeshTransport;

impl PodMeshTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

#[async_trait]
impl ReplicationTransport for PodMeshTransport {
    async fn list_peers(&self) -> Result<Vec<TransportPeer>> {
        let own_peer_id = system::host_identity::machine_id_short().to_string();
        let conn = db::open_default()?;
        let rows = pdb::list_peers(&conn)?;
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
        let envelope = sign_bundle(bundle.clone())?;
        let targets = dial_targets(peer);
        crate::dialer::try_targets(&targets, |t| {
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
        let envelope =
            crate::dialer::try_targets(
                &targets,
                |t| async move { fetch_replicate_bundle(&t).await },
            )
            .await?;
        verify_envelope(&envelope, pinned_fp)
    }

    async fn fetch_roots(&self, peer: &TransportPeer) -> Result<BTreeMap<String, String>> {
        let targets = dial_targets(peer);
        let r = crate::dialer::try_targets(
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
    match db::open_default() {
        Ok(conn) => crate::dialer::dial_targets_for_peer(&conn, &peer.peer_id, &peer.addr)
            .unwrap_or_else(|_| vec![peer.addr.clone()]),
        Err(_) => vec![peer.addr.clone()],
    }
}

/// Sign the given entities bundle with this host's bootstrap key. Shared by
/// push (transport) + receiver-side `pod/replicate-export` handler.
pub fn sign_bundle(entities: BTreeMap<String, Value>) -> Result<utils::pki::SignedEnvelope> {
    let body = ReplicateBundle {
        peer_id: system::host_identity::machine_id_short().to_string(),
        issued_at: utils::time::now().unix_seconds(),
        entities,
    };
    let signing = utils::pki::load_or_init_bootstrap_key(&pki_dir())?;
    utils::pki::sign_envelope(&signing, &body).context("sign replicate bundle")
}

/// Verify a signed envelope against the expected pinned bootstrap fp; return
/// the verified entities map. Used by transport.fetch + receiver-side
/// `pod/replicate-push` handler.
pub fn verify_envelope(
    envelope: &utils::pki::SignedEnvelope,
    pinned_fp: &str,
) -> Result<BTreeMap<String, Value>> {
    let (bundle, verifying) = utils::pki::verify_envelope::<ReplicateBundle>(envelope)
        .context("verify bundle envelope")?;
    let signer_fp = utils::pki::bootstrap_pubkey_fingerprint(&verifying);
    anyhow::ensure!(
        signer_fp == pinned_fp,
        "replicate bundle signer fp {signer_fp} does not match pinned peer fp {pinned_fp}"
    );
    Ok(bundle.entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_entities() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert("users".into(), Value::Array(vec![]));
        m
    }

    #[test]
    fn sign_then_verify_envelope_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let signing = utils::pki::load_or_init_bootstrap_key(tmp.path()).unwrap();
        let fp = utils::pki::bootstrap_pubkey_fingerprint(&signing.verifying_key());

        // sign_bundle uses pki_dir(); for the roundtrip we directly drive
        // utils::pki::sign_envelope so the test stays hermetic.
        let body = ReplicateBundle {
            peer_id: "test".into(),
            issued_at: 0,
            entities: empty_entities(),
        };
        let envelope = utils::pki::sign_envelope(&signing, &body).unwrap();
        let entities = verify_envelope(&envelope, &fp).unwrap();
        assert!(entities.contains_key("users"));
    }

    #[test]
    fn verify_envelope_rejects_wrong_fp() {
        let tmp = tempfile::tempdir().unwrap();
        let signing = utils::pki::load_or_init_bootstrap_key(tmp.path()).unwrap();
        let body = ReplicateBundle {
            peer_id: "test".into(),
            issued_at: 0,
            entities: empty_entities(),
        };
        let envelope = utils::pki::sign_envelope(&signing, &body).unwrap();
        let err = verify_envelope(&envelope, "not-the-actual-fp").unwrap_err();
        assert!(
            err.to_string().contains("does not match pinned"),
            "expected pinned-fp mismatch, got: {err}"
        );
    }
}
