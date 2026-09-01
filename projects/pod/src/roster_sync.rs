//! Auto-mesh: every paired peer periodically pulls the thin `pod.list`
//! membership from every other peer it knows about and merges joined entries
//! into its own `pod_peers`.
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
use std::time::{Duration, Instant};
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
        periodic::boxed(resync),
    )
}

/// One roster-sync pass: for every usable source peer the reachability gate
/// allows, dial its address set, ingest the roster it returns, and surface any
/// sustained-unreachable address to the operator. Runs on the 60s timer and is
/// also invoked directly by the down→up catch-up hook so a returning peer's
/// membership is refreshed at once.
pub async fn resync() -> Result<()> {
    let pki_d = pki_dir();
    // Gate: we need a mesh client cert to dial any peer. Hosts that haven't
    // completed initial pairing don't have one yet — let `pod-scheduler`
    // bootstrap them first.
    if utils::pki::load_mesh_client(&pki_d).is_err() {
        return Ok(());
    }

    let own_peer_id = system::host_identity::machine_id().to_string();

    // Build (source, ordered dial-targets) plans while the conn is alive.
    // Every source is dialed across ALL of its known addresses (LAN v4/v6,
    // Tailscale, fqdn, legacy) so a dual-homed peer whose primary interface is
    // momentarily unreachable is still reached via another — no more looping
    // forever on a single stale peer_addr.
    let plans: Vec<(pdb::PeerRow, Vec<String>)> = db::pool::with_pooled_or_open(|conn| {
        Ok(pdb::list_peers(conn)?
            .into_iter()
            .filter(|p| is_usable_source(p, &own_peer_id))
            .map(|p| {
                let targets = crate::dialer::dial_targets_for_peer(conn, &p.peer_id, &p.peer_addr)
                    .unwrap_or_else(|_| vec![p.peer_addr.clone()]);
                (p, targets)
            })
            .collect())
    })?;

    // Reclaim route-health entries for peers no longer in the membership (departed
    // / forgotten). The cache is append-only per dial, so without this sweep a
    // retired peer's `(peer_id,address)` rows would live forever. `plans` is the
    // current usable-source set; anything else is stale.
    let active: std::collections::HashSet<String> =
        plans.iter().map(|(p, _)| p.peer_id.clone()).collect();
    crate::route_health::retain_peers(&active);

    for (src, targets) in plans {
        // Skip a source the shared reachability source of truth says not to dial
        // (unreachable/backoff/dormant), so a fully-down peer isn't full-address-
        // swept every 60s tick. This is the same authority replicate.pull and the
        // liveness refresher honour.
        if !utils::reachability::should_dial(&src.peer_id, Instant::now()) {
            tracing::debug!(
                "[roster-sync] {} suppressed by reachability backoff",
                src.peer_hostname
            );
            continue;
        }
        // The probe dials every address of `src`; `try_targets_tracked` records
        // each per-address outcome into the route-health cache (this is the
        // reused 60s heartbeat — no separate prober).
        match fetch_roster_multi(&src.peer_id, &targets).await {
            Ok(out) => {
                // If this loop observed the down→up transition, fire the catch-up
                // hook (see replicate_engine for the "whichever sees it first"
                // rationale).
                if utils::reachability::record_probe(&src.peer_id, true, Instant::now())
                    .became_reachable
                {
                    utils::reachability::notify_reachable(&src.peer_id);
                }
                match ingest_roster(&own_peer_id, &src.peer_hostname, out).await {
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
                }
            }
            Err(e) => {
                utils::reachability::record_probe(&src.peer_id, false, Instant::now());
                let fails = utils::reachability::reachability(&src.peer_id)
                    .map(|s| s.consecutive_failures)
                    .unwrap_or(1);
                if fails <= 1 {
                    warn!(
                        "[roster-sync] fetch from {} failed: {e:#} — backing off",
                        src.peer_hostname
                    );
                } else {
                    tracing::debug!(
                        "[roster-sync] {} still failing — {fails} consecutive failures",
                        src.peer_hostname
                    );
                }
            }
        }
        // Any address of this peer that's been continuously unreachable past the
        // sustained threshold surfaces to the operator once, with a suppress
        // remediation. Reachability is directional, so the alert is framed
        // "from this host".
        notify_stale_routes(&own_peer_id, &src).await;
    }
    Ok(())
}

/// Raise a one-shot operator notification for each of `src`'s addresses that has
/// been unreachable from this host past the sustained threshold. Best-effort:
/// notification-emit failures never disrupt the roster tick.
async fn notify_stale_routes(own_peer_id: &str, src: &pdb::PeerRow) {
    for stale in crate::route_health::take_stale_routes(&src.peer_id) {
        let title = format!(
            "peer {} address {} unreachable",
            src.peer_hostname, stale.addr
        );
        let body = format!(
            "Address {} for peer {} ({}) has been unreachable from this host ({}) \
             for ~{} min ({} consecutive failed dials). Other addresses may still \
             be routable. Reachability is per-vantage — this address may be fine \
             from other peers. Recommended: suppress this address for this host's \
             dial set (a local override; autodetect would otherwise re-add it).",
            stale.addr,
            src.peer_hostname,
            src.peer_id,
            own_peer_id,
            stale.minutes_down,
            stale.consecutive_failures,
        );
        let event = notifications::Event::new(
            notifications::EventClass::Alert,
            notifications::Severity::Warn,
            title,
            "pod:route_health",
        )
        .with_body(body)
        .with_host(src.peer_hostname.clone());
        // Fire-and-forget: emit returns per-backend outcomes; we don't gate the
        // tick on delivery.
        let _ = notifications::emit(&event).await;
        warn!(
            "[route-health] {} addr {} unreachable {}m — notified operator",
            src.peer_hostname, stale.addr, stale.minutes_down
        );
    }
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
/// Pick a dial address for a roster entry. Post-collapse peers stop sending a
/// top-level `addr`, so fall back to the channel list: prefer a LAN IPv4, then
/// any non-empty channel value. Empty string only if the entry carries nothing
/// dialable (upsert then no-ops on a blank addr).
fn entry_primary_addr(entry: &PodPeerDto) -> String {
    if !entry.addr.is_empty() {
        return entry.addr.clone();
    }
    entry
        .routes
        .iter()
        .find(|a| a.kind == crate::dialer::LAN_V4)
        .or_else(|| entry.routes.iter().find(|a| !a.value.is_empty()))
        .map(|a| a.value.clone())
        .unwrap_or_default()
}

async fn fetch_roster_multi(peer_id: &str, targets: &[String]) -> Result<Vec<PodPeerDto>> {
    crate::dialer::try_targets_tracked(
        Some(peer_id),
        targets,
        |t| async move { fetch_roster(&t).await },
    )
    .await
}

async fn fetch_roster(addr: &str) -> Result<Vec<PodPeerDto>> {
    // `pod.list` is now the thin raw-membership roster (no enrichment fan-out),
    // so roster-sync reads it directly. On peers still in the rolling-upgrade
    // window `pod.list` returns the older enriched shape — a superset that
    // deserializes fine here, since roster ingest only reads identity/address.
    let empty = || serde_json::Value::Object(Default::default());
    let result = super::exec(addr, "pod.list", empty()).await?;
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

    db::pool::with_pooled_or_open(|conn| {
        let mut added = 0;
        for entry in list {
            // Full-uuid identity is a hard invariant: never learn a peer under a
            // short/legacy/prefixed id form. A pre-uuidv7 CN (`019e7105-991`,
            // `c56ccc7c2039`) or a `peer.<id>` prefix would otherwise land as a
            // SEPARATE pod_peers row that convergence can't fold back onto the
            // canonical row — the exact split that scrambled the roster. Drop it
            // loudly rather than persist a second-class identity.
            if !utils::id::is_uuidv7(&entry.peer_id) {
                warn!(
                    "[roster-sync] {} → dropping non-uuidv7 peer_id {:?} for {:?}: full-uuid identity required",
                    source_label, entry.peer_id, entry.hostname
                );
                continue;
            }
            if !is_ingestable(&entry, own_peer_id) {
                continue;
            }
            // Resurrection guard (issue #232): a forgotten peer carries a durable,
            // replicated tombstone. Even if a straggler that missed the original
            // `pod/peer-forget` fan-out still lists this peer as "active", skip the
            // upsert so the forget is not undone. The tombstone is TTL-bounded, so a
            // genuinely re-pairing host (new uuidv7 identity) is unaffected.
            if pdb::is_peer_forgotten(conn, &entry.peer_id).unwrap_or(false) {
                continue;
            }
            // Post-collapse peers no longer serialize a top-level `addr`; derive a
            // dial address from the channel list instead (the DB still stores one
            // primary peer_addr, and pod/ping fills in the full multi-address set).
            let addr = entry_primary_addr(&entry);
            let prior_fp = pdb::peer_pubkey_fp_raw(conn, &entry.peer_id)?;
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
                conn,
                &entry.peer_id,
                &entry.hostname,
                &addr,
                entry.port,
                entry.pubkey_fp.as_deref(),
                &ca_cert_pem,
            )?;
            // Converge on the write path: if this ingest just wrote a divergent id
            // form (legacy `peer.<id>` vs bare, or a re-keyed identity at the same
            // address) for a host we already track, fold the rows into one canonical
            // row NOW — otherwise roster-sync re-creates the duplicate every cycle,
            // out-pacing the boot/handshake cleanup passes.
            match pdb::converge_peer_identity(conn, &entry.peer_id, &addr) {
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
                        addr,
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
    })
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
            routes: Default::default(),
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

    // ── entry_primary_addr ───────────────────────────────────────────────────

    fn routes_of(pairs: &[(&str, &str)]) -> utils::route::Routes {
        let mut r = utils::route::Routes::new();
        for (kind, value) in pairs {
            r.push(utils::route::Route::mesh(*kind, *value, None));
        }
        r
    }

    #[test]
    fn primary_addr_prefers_top_level_addr() {
        // A pre-collapse peer still ships a top-level `addr`; it wins outright,
        // routes are not consulted.
        let mut e = entry("other", "active", false);
        e.addr = "192.168.1.9".into();
        e.routes = routes_of(&[(crate::dialer::LAN_V4, "10.0.0.9")]);
        assert_eq!(entry_primary_addr(&e), "192.168.1.9");
    }

    #[test]
    fn primary_addr_falls_back_to_lan_v4_route() {
        // Post-collapse: no top-level addr, so prefer the LAN v4 channel even
        // when a different channel appears first.
        let mut e = entry("other", "active", false);
        e.addr = String::new();
        e.routes = routes_of(&[("fqdn", "host.lan"), (crate::dialer::LAN_V4, "10.0.0.7")]);
        assert_eq!(entry_primary_addr(&e), "10.0.0.7");
    }

    #[test]
    fn primary_addr_falls_back_to_first_nonempty_route() {
        // No addr and no LAN v4 → first channel with a non-empty value.
        let mut e = entry("other", "active", false);
        e.addr = String::new();
        e.routes = routes_of(&[("fqdn", "host.lan"), ("tailscale", "100.64.0.1")]);
        assert_eq!(entry_primary_addr(&e), "host.lan");
    }

    #[test]
    fn primary_addr_empty_when_nothing_dialable() {
        // No addr, no routes at all → empty string (upsert no-ops on it).
        let mut e = entry("other", "active", false);
        e.addr = String::new();
        e.routes = utils::route::Routes::new();
        assert_eq!(entry_primary_addr(&e), "");
    }

    #[test]
    fn primary_addr_skips_empty_valued_routes() {
        // A channel carrying an empty value is not dialable; with no other
        // usable channel the result is empty.
        let mut e = entry("other", "active", false);
        e.addr = String::new();
        e.routes = routes_of(&[("fqdn", "")]);
        assert_eq!(entry_primary_addr(&e), "");
    }

    // ── ingest_roster (DB + PKI round-trip) ──────────────────────────────────

    const FAKE_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----\n";
    const UUID_A: &str = "019e7105-0000-7000-8000-0000000abc01";
    const UUID_B: &str = "019e7105-0000-7000-8000-0000000abc02";

    /// Run `body` with HOME repointed at `home`, a mesh CA cert planted under the
    /// resulting pki dir, and a live current-thread runtime. Restores HOME after.
    fn with_prepared_home<T>(
        home: &std::path::Path,
        body: impl FnOnce(&tokio::runtime::Runtime) -> T,
    ) -> T {
        // Serialize behind the CRATE-WIDE HOME lock so this cannot race a
        // cert_rotation or cli test that also repoints HOME. Poison-tolerant.
        let _guard = crate::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("HOME").ok();
        // SAFETY: HOME is set for the closure's duration and restored right after;
        // access is serialized behind ENV_LOCK.
        unsafe { std::env::set_var("HOME", home) };
        let pki = pki_dir();
        std::fs::create_dir_all(utils::pki::mesh_dir(&pki)).unwrap();
        std::fs::write(utils::pki::mesh_ca_cert_path(&pki), FAKE_CA_PEM).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = body(&rt);
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    fn uuid_entry(peer_id: &str, hostname: &str) -> PodPeerDto {
        let mut e = entry(peer_id, "active", false);
        e.hostname = hostname.into();
        e
    }

    #[test]
    fn ingest_learns_new_active_peers_and_persists_them() {
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                let list = vec![uuid_entry(UUID_A, "alpha"), uuid_entry(UUID_B, "bravo")];
                let added = ingest_roster("me", "src-host", list).await.unwrap();
                assert_eq!(added, 2, "both fresh peers are newly learned");

                let conn = db::open_default().unwrap();
                let peers = pdb::list_peers(&conn).unwrap();
                assert!(peers.iter().any(|p| p.peer_id == UUID_A));
                assert!(peers.iter().any(|p| p.peer_id == UUID_B));
            }));
        });
    }

    #[test]
    fn ingest_is_idempotent_on_second_pass() {
        // Re-ingesting an already-known peer adds nothing (prior_fp is Some).
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                let first = ingest_roster("me", "src", vec![uuid_entry(UUID_A, "alpha")])
                    .await
                    .unwrap();
                assert_eq!(first, 1);
                let second = ingest_roster("me", "src", vec![uuid_entry(UUID_A, "alpha")])
                    .await
                    .unwrap();
                assert_eq!(second, 0, "already-known peer is not re-counted");
            }));
        });
    }

    #[test]
    fn ingest_drops_non_uuidv7_and_filtered_entries() {
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                let list = vec![
                    // Legacy short id — rejected by the full-uuid invariant.
                    uuid_entry("019e7105-991", "legacy"),
                    // Inactive — filtered by is_ingestable.
                    {
                        let mut e = uuid_entry(UUID_B, "departed-host");
                        e.status = "departed".into();
                        e
                    },
                    // Self — filtered by is_ingestable.
                    uuid_entry("me", "myself"),
                ];
                let added = ingest_roster("me", "src", list).await.unwrap();
                assert_eq!(added, 0, "no admissible peers in the batch");

                let conn = db::open_default().unwrap();
                let peers = pdb::list_peers(&conn).unwrap();
                assert!(
                    !peers.iter().any(|p| p.peer_id == UUID_B),
                    "inactive entry was not persisted"
                );
            }));
        });
    }

    #[test]
    fn ingest_skips_forgotten_peer_tombstone() {
        // A peer with an active forget-tombstone must not be resurrected even if
        // a straggler still lists it as active (issue #232).
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                {
                    let conn = db::open_default().unwrap();
                    pdb::forget_peer(&conn, UUID_A).unwrap();
                }
                let added = ingest_roster("me", "src", vec![uuid_entry(UUID_A, "alpha")])
                    .await
                    .unwrap();
                assert_eq!(added, 0, "forgotten peer is not re-learned");

                let conn = db::open_default().unwrap();
                assert!(
                    !pdb::list_peers(&conn)
                        .unwrap()
                        .iter()
                        .any(|p| p.peer_id == UUID_A),
                    "tombstoned peer was not re-inserted"
                );
            }));
        });
    }

    #[test]
    fn ingest_transitive_pin_skips_when_fp_absent() {
        // A peer we already track with a pinned fp must not be re-processed when
        // the incoming roster entry carries no fp — the local pin stays intact.
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                {
                    let conn = db::open_default().unwrap();
                    pdb::upsert_peer(
                        &conn,
                        UUID_A,
                        "alpha",
                        "10.0.0.1",
                        12002,
                        Some("aa:bb"),
                        FAKE_CA_PEM,
                    )
                    .unwrap();
                }
                let mut e = uuid_entry(UUID_A, "alpha");
                e.pubkey_fp = None;
                let added = ingest_roster("me", "src", vec![e]).await.unwrap();
                assert_eq!(added, 0, "already-pinned peer is not re-counted");

                let conn = db::open_default().unwrap();
                assert_eq!(
                    pdb::peer_pubkey_fp_raw(&conn, UUID_A).unwrap(),
                    Some(Some("aa:bb".to_string())),
                    "existing pin is preserved, not clobbered"
                );
            }));
        });
    }

    #[test]
    fn ingest_backfills_pubkey_fp_for_known_peer() {
        // A peer known without a fp gets its pin backfilled when the roster entry
        // now carries one; it is not counted as newly learned.
        let home = tempfile::tempdir().unwrap();
        with_prepared_home(home.path(), |rt| {
            let db_file = tempfile::NamedTempFile::new().unwrap();
            rt.block_on(db::with_db_path(db_file.path().to_path_buf(), async {
                {
                    let conn = db::open_default().unwrap();
                    pdb::upsert_peer(&conn, UUID_A, "alpha", "10.0.0.1", 12002, None, FAKE_CA_PEM)
                        .unwrap();
                    assert_eq!(
                        pdb::peer_pubkey_fp_raw(&conn, UUID_A).unwrap(),
                        Some(None),
                        "precondition: known peer without a fp"
                    );
                }
                let mut e = uuid_entry(UUID_A, "alpha");
                e.pubkey_fp = Some("cc:dd".into());
                let added = ingest_roster("me", "src", vec![e]).await.unwrap();
                assert_eq!(added, 0, "backfill is not a new learn");

                let conn = db::open_default().unwrap();
                assert_eq!(
                    pdb::peer_pubkey_fp_raw(&conn, UUID_A).unwrap(),
                    Some(Some("cc:dd".to_string())),
                    "pubkey_fp was backfilled onto the existing row"
                );
            }));
        });
    }
}
