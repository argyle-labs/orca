//! Shared-state replication: every paired peer periodically pulls every other
//! peer's `pod/replicate-export` bundle and merges it locally. Generic over all
//! entities registered via `#[derive(Replicated)]` — `users` today, configs +
//! settings later — through one signed bundle per peer.
//!
//! `users` is ONE shared pool: every user accessible on every host, writable by
//! ANY paired host, converging last-write-wins. So any admin can sign in on any
//! machine/UI. See project_unified_mesh_state.md.
//!
//! **Trust:** the bundle is signed with the source host's bootstrap key. We
//! verify the signature AND require the signer fp to equal the source peer's
//! *pinned* `pod_peers.pubkey_fp` before merging — a valid sig from an unpinned
//! key is rejected. This authenticates the transport; it is not per-row
//! ownership (shared pools have no owner). See feedback_zero_trust_no_blind_trust.md.

use crate::native::ReplicateBundle;
use anyhow::{Context, Result};
use orca_sdk::pki;
use std::time::Duration;
use tracing::{info, warn};

use super::{db as pdb, fetch_replicate_bundle, pki_dir};
use system::periodic;

const TICK_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.replication_sync.run",
            initial_delay: Duration::from_secs(25),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(tick),
    )
}

async fn tick() -> Result<()> {
    let pki_d = pki_dir();
    // Need a mesh client cert to dial any peer.
    if pki::load_mesh_client(&pki_d).is_err() {
        return Ok(());
    }

    let own_peer_id = format!("peer.{}", system::host_identity::machine_id_short());

    let peers = {
        let conn = db::open_default()?;
        pdb::list_peers(&conn)?
    };

    for src in peers {
        if !is_usable_source(&src, &own_peer_id) {
            continue;
        }
        let pinned_fp = match &src.pubkey_fp {
            Some(fp) => fp.clone(),
            // No pinned bootstrap fp → we can't authenticate the bundle's
            // origin. Skip rather than trust an unpinned payload.
            None => continue,
        };
        match fetch_replicate_bundle(&src.peer_addr).await {
            Ok(env) => match merge_bundle(&env, &pinned_fp) {
                Ok(merged) if merged > 0 => {
                    info!(
                        "[replication-sync] merged {merged} row(s) from {}",
                        src.peer_hostname
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "[replication-sync] merge from {} failed: {e:#}",
                    src.peer_hostname
                ),
            },
            Err(e) => warn!(
                "[replication-sync] fetch from {} failed: {e:#}",
                src.peer_hostname
            ),
        }
    }
    Ok(())
}

/// Same source filter as roster-sync: skip departed peers, legacy `unknown`
/// stubs, and self.
fn is_usable_source(p: &pdb::PeerRow, own_peer_id: &str) -> bool {
    if p.departed_at.is_some() {
        return false;
    }
    if p.peer_id == "unknown" || p.peer_id == own_peer_id {
        return false;
    }
    true
}

/// Verify the signed bundle against the source's pinned bootstrap fp, then
/// dispatch each entity to its registered LWW merge. Returns rows merged.
fn merge_bundle(env: &pki::SignedEnvelope, pinned_fp: &str) -> Result<usize> {
    let (bundle, verifying) =
        pki::verify_envelope::<ReplicateBundle>(env).context("verify replicate bundle envelope")?;
    let signer_fp = pki::bootstrap_pubkey_fingerprint(&verifying);
    anyhow::ensure!(
        signer_fp == pinned_fp,
        "replicate bundle signer fp {signer_fp} does not match pinned peer fp {pinned_fp}"
    );

    let conn = db::open_default()?;
    replicate::merge_bundle(&conn, bundle.entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_row(peer_id: &str, departed: bool, fp: Option<&str>) -> pdb::PeerRow {
        pdb::PeerRow {
            peer_id: peer_id.into(),
            peer_hostname: "h".into(),
            peer_addr: "10.0.0.1".into(),
            peer_port: 12002,
            pubkey_fp: fp.map(|s| s.to_string()),
            first_seen_at: 0,
            last_seen_at: 0,
            departed_at: if departed { Some(1) } else { None },
            local_secure: true,
            peer_secure: true,
        }
    }

    #[test]
    fn source_active_real_peer_is_usable() {
        assert!(is_usable_source(
            &peer_row("peer.real", false, Some("fp")),
            "peer.me"
        ));
    }

    #[test]
    fn source_departed_is_skipped() {
        assert!(!is_usable_source(
            &peer_row("peer.real", true, Some("fp")),
            "peer.me"
        ));
    }

    #[test]
    fn source_self_is_skipped() {
        assert!(!is_usable_source(
            &peer_row("peer.me", false, Some("fp")),
            "peer.me"
        ));
    }

    #[test]
    fn source_unknown_stub_is_skipped() {
        assert!(!is_usable_source(
            &peer_row("unknown", false, Some("fp")),
            "peer.me"
        ));
    }

    #[test]
    fn merge_rejects_wrong_signer_fp() {
        let dir = tempfile::tempdir().unwrap();
        let signing = pki::load_or_init_bootstrap_key(dir.path()).unwrap();
        let body = ReplicateBundle {
            peer_id: "peer.src".into(),
            issued_at: 0,
            entities: std::collections::BTreeMap::new(),
        };
        let env = pki::sign_envelope(&signing, &body).unwrap();
        let err = merge_bundle(&env, "not-the-signer-fp").unwrap_err();
        assert!(err.to_string().contains("does not match pinned"), "{err}");
    }
}
