//! Shared-users replication: every paired peer periodically pulls every other
//! peer's `pod/users-export` and merges it into its own `users` table.
//!
//! `users` is ONE shared pool — every user accessible on every host, writable
//! by ANY paired host, converging last-write-wins on `updated_at`. So any admin
//! can sign in on any machine/UI. See project_unified_mesh_state.md.
//!
//! **Trust:** the export is signed with the source host's bootstrap key. We
//! verify the signature AND require the signer fp to equal the source peer's
//! *pinned* `pod_peers.pubkey_fp` before merging a single row — a valid sig
//! from an unpinned key is rejected. This authenticates the transport; it is
//! not per-user ownership (the pool has no owner). See
//! feedback_zero_trust_no_blind_trust.md.

use crate::UsersExport;
use anyhow::{Context, Result};
use orca_sdk::pki;
use std::time::Duration;
use tracing::{info, warn};

use super::{db as pdb, fetch_users_export, pki_dir};
use system::periodic;

const TICK_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.users_sync.run",
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
            // No pinned bootstrap fp → we can't authenticate the export's
            // origin. Skip rather than trust an unpinned payload.
            None => continue,
        };
        match fetch_users_export(&src.peer_addr).await {
            Ok(env) => match merge_export(&env, &pinned_fp) {
                Ok(merged) if merged > 0 => {
                    info!(
                        "[users-sync] merged {merged} user(s) from {}",
                        src.peer_hostname
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "[users-sync] merge from {} failed: {e:#}",
                    src.peer_hostname
                ),
            },
            Err(e) => warn!(
                "[users-sync] fetch from {} failed: {e:#}",
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

/// Verify the signed export against the source's pinned bootstrap fp, then
/// upsert every row last-write-wins. Returns the number of rows created/updated.
fn merge_export(env: &pki::SignedEnvelope, pinned_fp: &str) -> Result<usize> {
    let (export, verifying) =
        pki::verify_envelope::<UsersExport>(env).context("verify users export envelope")?;
    let signer_fp = pki::bootstrap_pubkey_fingerprint(&verifying);
    anyhow::ensure!(
        signer_fp == pinned_fp,
        "users export signer fp {signer_fp} does not match pinned peer fp {pinned_fp}"
    );

    let conn = db::open_default()?;
    let mut merged = 0;
    for u in &export.users {
        match db::users::upsert_replica(&conn, u) {
            Ok(true) => merged += 1,
            Ok(false) => {}
            // A single bad row (e.g. username_lower collision) must not abort
            // the whole merge — log and continue.
            Err(e) => warn!("[users-sync] skip user {}: {e:#}", u.id),
        }
    }
    Ok(merged)
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
        let body = UsersExport {
            peer_id: "peer.src".into(),
            issued_at: 0,
            users: vec![],
        };
        let env = pki::sign_envelope(&signing, &body).unwrap();
        // Pinned fp is some other peer's fp → must reject.
        let err = merge_export(&env, "not-the-signer-fp").unwrap_err();
        assert!(err.to_string().contains("does not match pinned"), "{err}");
    }
}
