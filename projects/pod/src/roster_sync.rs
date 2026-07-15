//! Auto-mesh: every paired peer periodically pulls `pod.list` from every other
//! peer it knows about and merges joined entries into its own `pod_peers`.
//! The result is an eventually-consistent full mesh from any starting
//! topology — once one peer in the pod knows about a new joiner, the next
//! tick propagates that fact to every other peer.
//!
//! **Why this works without a CA private-key signing roundtrip**: the mesh
//! CA cert is already on every paired host (it was delivered with the
//! initial pairing offer). Any peer with a CA-signed cert can mTLS-dial
//! any other peer with a CA-signed cert. The only thing missing in a
//! star-topology mesh is each peer's *knowledge* of the others' addresses —
//! which is exactly what this task fills in.
//!
//! **What this task does NOT do**: re-pair, sign new certs, replicate
//! secrets. Those are separate flows. This is read-only address/identity
//! discovery on top of an already-trusted CA.
//!
//! Compose with the accept-side bugfix (separate slice): once `pod accept`
//! records the real `peer_id` (not `"unknown"`), this loop converges in
//! seconds; before that fix, peers with `peer_id="unknown"` are skipped
//! both as sources and as merge targets.

use crate::{PodListOutput, PodMember, PodPeerDto};
use anyhow::Result;
use std::time::Duration;
use tracing::{info, warn};

use super::pki_dir;
use db::pod as pdb;
use system::periodic;

const TICK_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn() -> tokio::task::JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "pod.roster_sync.run",
            // Small initial delay so we don't slam the peers on every restart.
            initial_delay: Duration::from_secs(20),
            interval: TICK_INTERVAL,
        },
        periodic::boxed(tick),
    )
}

async fn tick() -> Result<()> {
    let pki_d = pki_dir();
    // Gate: we need a mesh client cert to dial any peer. Hosts that haven't
    // completed initial pairing don't have one yet — let `pod-scheduler`
    // bootstrap them first.
    if utils::pki::load_mesh_client(&pki_d).is_err() {
        return Ok(());
    }

    let own_peer_id = system::host_identity::machine_id_short().to_string();

    // Build (source, ordered dial-targets) plans while the conn is alive.
    // Every source is dialed across ALL of its known addresses (LAN v4/v6,
    // Tailscale, fqdn, legacy) so a dual-homed peer whose primary interface is
    // momentarily unreachable is still reached via another — no more looping
    // forever on a single stale peer_addr.
    let plans: Vec<(pdb::PeerRow, Vec<String>)> = {
        let conn = db::open_default()?;
        pdb::list_peers(&conn)?
            .into_iter()
            .filter(|p| is_usable_source(p, &own_peer_id))
            .map(|p| {
                let targets = crate::dialer::dial_targets_for_peer(&conn, &p.peer_id, &p.peer_addr)
                    .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                (p, targets)
            })
            .collect()
    };

    for (src, targets) in plans {
        match fetch_roster_multi(&targets).await {
            Ok(out) => match ingest_roster(&own_peer_id, &src.peer_hostname, out).await {
                Ok(added) if added > 0 => {
                    info!(
                        "[roster-sync] learned {added} peer(s) from {}",
                        src.peer_hostname
                    );
                }
                Ok(_) => {}
                Err(e) => warn!(
                    "[roster-sync] ingest from {} failed: {e:#}",
                    src.peer_hostname
                ),
            },
            Err(e) => warn!(
                "[roster-sync] fetch from {} failed: {e:#}",
                src.peer_hostname
            ),
        }
    }
    Ok(())
}

/// True if this row is something we should poll for a roster. We skip:
/// - departed peers (no point dialing)
/// - the legacy `"unknown"` stub left over from rc.≤24 pairings — these
///   have no usable peer_id and the dial would fail anyway
/// - rows that point back at ourselves (e.g. self-discovered via mDNS) —
///   we already know our own roster
fn is_usable_source(p: &pdb::PeerRow, own_peer_id: &str) -> bool {
    if p.departed_at.is_some() {
        return false;
    }
    if p.peer_id == "unknown" || p.peer_id == own_peer_id {
        return false;
    }
    true
}

/// True if a roster entry from a remote peer is something we should ingest
/// into our local `pod_peers`. Same filters as `is_usable_source`, plus:
/// - the synthetic `local` row in the remote's response (that's the
///   remote peer itself — we already have it as the source)
/// - inactive entries (the remote may carry departed rows for history)
pub(crate) fn is_ingestable(entry: &PodPeerDto, own_peer_id: &str) -> bool {
    if entry.local {
        return false;
    }
    if entry.peer_id == "local" || entry.peer_id == "unknown" || entry.peer_id == own_peer_id {
        return false;
    }
    if entry.status != "active" {
        return false;
    }
    true
}

/// Dial a source across every known address, returning the first roster we
/// successfully fetch. Falls through the ordered target list on connect error.
async fn fetch_roster_multi(targets: &[String]) -> Result<Vec<PodPeerDto>> {
    crate::dialer::try_targets(targets, |t| async move { fetch_roster(&t).await }).await
}

async fn fetch_roster(addr: &str) -> Result<Vec<PodPeerDto>> {
    let result = super::exec(
        addr,
        "pod.list",
        serde_json::Value::Object(Default::default()),
    )
    .await?;
    let list: PodListOutput = serde_json::from_value(result.result)?;
    // Auto-mesh only consumes paired members; handshaking + discovered rows
    // are surfaced for UI/operator use, not for address propagation.
    Ok(list
        .members
        .into_iter()
        .filter_map(|m| match m {
            PodMember::Joined(p) => Some(*p),
            _ => None,
        })
        .collect())
}

async fn ingest_roster(
    own_peer_id: &str,
    source_label: &str,
    list: Vec<PodPeerDto>,
) -> Result<usize> {
    let pki_d = pki_dir();
    let ca_cert_pem = std::fs::read_to_string(utils::pki::mesh_ca_cert_path(&pki_d))?;
    let conn = db::open_default()?;

    let mut added = 0;
    for entry in list {
        if !is_ingestable(&entry, own_peer_id) {
            continue;
        }
        let prior_fp = pdb::peer_pubkey_fp_raw(&conn, &entry.peer_id)?;
        // Transitive pin: if the source peer published a `pubkey_fp` for this
        // entry (they paired directly), forward it so we can pin too — without
        // this, every cross-host pod/exec from a roster-learned peer is
        // refused with "no pinned bootstrap key to verify against". The
        // COALESCE in upsert_peer keeps a directly-pinned fp from being
        // clobbered if it was already set locally.
        if prior_fp.is_some() && entry.pubkey_fp.is_none() {
            continue;
        }
        pdb::upsert_peer(
            &conn,
            &entry.peer_id,
            &entry.hostname,
            &entry.addr,
            entry.port,
            entry.pubkey_fp.as_deref(),
            &ca_cert_pem,
        )?;
        // Converge on the write path: if this ingest just wrote a divergent id
        // form (legacy `peer.<id>` vs bare, or a re-keyed identity at the same
        // address) for a host we already track, fold the rows into one canonical
        // row NOW — otherwise roster-sync re-creates the duplicate every cycle,
        // out-pacing the boot/handshake cleanup passes.
        match pdb::converge_peer_identity(&conn, &entry.peer_id, &entry.addr) {
            Ok(0) => {}
            Ok(n) => info!(
                "[roster-sync] {} → converged {} duplicate row(s) for {} onto one canonical identity",
                source_label, n, entry.hostname
            ),
            Err(e) => warn!(
                "[roster-sync] {} → identity convergence for {} failed: {e}",
                source_label, entry.hostname
            ),
        }
        match &prior_fp {
            None => {
                added += 1;
                info!(
                    "[roster-sync] {} → learned {} ({}, {}:{}, pinned={})",
                    source_label,
                    entry.hostname,
                    entry.peer_id,
                    entry.addr,
                    entry.port,
                    entry.pubkey_fp.is_some()
                );
            }
            Some(None) if entry.pubkey_fp.is_some() => {
                info!(
                    "[roster-sync] {} → backfilled pubkey_fp for {} ({})",
                    source_label, entry.hostname, entry.peer_id
                );
            }
            _ => {}
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_row(peer_id: &str, departed: bool) -> pdb::PeerRow {
        pdb::PeerRow {
            peer_id: peer_id.into(),
            peer_hostname: "h".into(),
            peer_addr: "10.0.0.1".into(),
            peer_port: 12002,
            pubkey_fp: None,
            first_seen_at: 0,
            last_seen_at: 0,
            departed_at: if departed { Some(1) } else { None },
            local_secure: true,
            peer_secure: true,
        }
    }

    fn entry(peer_id: &str, status: &str, local: bool) -> PodPeerDto {
        PodPeerDto {
            peer_id: peer_id.into(),
            hostname: "h".into(),
            addr: "10.0.0.1".into(),
            port: 12002,
            last_seen_at: 0,
            local_secure: false,
            peer_secure: false,
            status: status.into(),
            addresses: vec![],
            local,
            reachable: None,
            latency_ms: None,
            probe_error: None,
            version: None,
            target: None,
            frontend: None,
            mode: None,
            channel: None,
            pinned_to: None,
            update_latest: None,
            update_available: None,
            update_checked_secs: None,
            system: None,
            pubkey_fp: None,
        }
    }

    // ── is_usable_source ─────────────────────────────────────────────────────

    #[test]
    fn source_active_real_peer_is_usable() {
        assert!(is_usable_source(&peer_row("real", false), "me"));
    }

    #[test]
    fn source_departed_is_skipped() {
        assert!(!is_usable_source(&peer_row("real", true), "me"));
    }

    #[test]
    fn source_unknown_stub_is_skipped() {
        // The rc.≤24 legacy stubs have peer_id="unknown" and no usable
        // bootstrap channel — skip rather than waste a dial.
        assert!(!is_usable_source(&peer_row("unknown", false), "me"));
    }

    #[test]
    fn source_self_is_skipped() {
        // Self-discovered (mDNS picked up our own LAN addr) — no point
        // dialing ourselves for a roster.
        assert!(!is_usable_source(&peer_row("me", false), "me"));
    }

    // ── is_ingestable ────────────────────────────────────────────────────────

    #[test]
    fn ingest_active_real_peer() {
        assert!(is_ingestable(&entry("other", "active", false), "me"));
    }

    #[test]
    fn ingest_skips_synthetic_local_row() {
        // The remote's response includes its own row with local=true; that's
        // the source peer (which we already have) and must not be merged.
        assert!(!is_ingestable(&entry("other", "active", true), "me"));
    }

    #[test]
    fn ingest_skips_synthetic_local_peer_id() {
        // Defense in depth — even if `local` flag is missing, peer_id="local"
        // is the synthetic marker.
        assert!(!is_ingestable(&entry("local", "active", false), "me"));
    }

    #[test]
    fn ingest_skips_unknown_stub() {
        assert!(!is_ingestable(&entry("unknown", "active", false), "me"));
    }

    #[test]
    fn ingest_skips_self() {
        assert!(!is_ingestable(&entry("me", "active", false), "me"));
    }

    #[test]
    fn ingest_skips_inactive() {
        assert!(!is_ingestable(&entry("other", "departed", false), "me"));
    }
}
