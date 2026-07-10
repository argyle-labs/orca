//! Periodic per-peer `system.update {}` probe.
//!
//! Every 60s, for each active paired peer, this task issues a READ-ONLY
//! `system.update {}` over the pod mesh and persists the result in
//! `peer_update_state`. `pod.list` joins that table when building peer
//! entries so the web UI's per-peer drawer + card show *that peer's*
//! version/channel/pin/update-available — not the local daemon's.
//!
//! ── SAFETY ────────────────────────────────────────────────────────────────
//! This probe MUST send `{}` and nothing else. `system.update` is the same
//! tool surface used to apply updates, switch channels, pin/unpin, and
//! reissue host addressing. A non-empty arg here would mutate every peer
//! every minute — catastrophic. If you need to mutate a peer's update
//! state, do it from the user-facing tool path with an explicit caller
//! identity, not from this loop.
//!
//! Failures are logged at debug and DO NOT poison the row — the last good
//! values stay in place; only `checked_at` advances on success.

use anyhow::{Context, Result};
use std::time::Duration;
use system::periodic::{PeriodicSpec, spawn};

/// Cadence between probe ticks. Matches the host_status puller's slow
/// cadence — slower than the live-subscribe path because the update state
/// only changes when an operator applies an update or a new release lands.
const PROBE_INTERVAL_SECS: u64 = 60;

/// Per-peer call budget. Generous because some peers' GitHub list-versions
/// fan-out can take a few seconds; we'd rather skip a tick than truncate a
/// slow probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub fn spawn_periodic() {
    use std::sync::OnceLock;
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::mem::drop(spawn(
        PeriodicSpec {
            name: "pod.peer_update_probe.tick",
            initial_delay: Duration::from_secs(10),
            interval: Duration::from_secs(PROBE_INTERVAL_SECS),
        },
        system::periodic::boxed(probe_tick),
    ));
}

async fn probe_tick() -> Result<()> {
    let peers = tokio::task::spawn_blocking(|| -> Result<Vec<(String, String)>> {
        let conn = db::open_default()?;
        let rows = db::pod::list_peer_summaries(&conn)?;
        let own = system::host_identity::machine_id_short().to_string();
        Ok(rows
            .into_iter()
            .filter(|p| p.status == "active" && p.peer_id != own)
            .map(|p| (p.peer_id, p.addr))
            .collect())
    })
    .await??;

    let mut handles = Vec::with_capacity(peers.len());
    for (peer_id, addr) in peers {
        handles.push(tokio::spawn(probe_one(peer_id, addr)));
    }
    for h in handles {
        if let Err(e) = h.await {
            tracing::debug!("peer_update_probe join: {e:#}");
        }
    }
    Ok(())
}

async fn probe_one(peer_id: String, addr: String) {
    match tokio::time::timeout(PROBE_TIMEOUT, probe_one_inner(&peer_id, &addr)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!("peer_update_probe {peer_id} ({addr}): {e:#}"),
        Err(_) => tracing::debug!("peer_update_probe {peer_id} ({addr}): timeout"),
    }
}

async fn probe_one_inner(peer_id: &str, addr: &str) -> Result<()> {
    // READ-ONLY probe — `{}` only. See module-level SAFETY note.
    let res = crate::exec(addr, "system.update", serde_json::json!({})).await?;
    let out: system::commands::SystemUpdateOutput =
        serde_json::from_value(res.result).context("decode SystemUpdateOutput")?;

    // `current_version` is canonical (no "v" prefix). `latest` may carry
    // a leading "v" from the GitHub release tag — keep it as-reported so
    // the UI can render the badge verbatim.
    let version = (!out.current_version.is_empty()).then_some(out.current_version.clone());
    let channel = (!out.channel.is_empty()).then_some(out.channel.clone());
    let latest = out.latest.clone();
    // Prefer the peer's own server-side computed flag so list-view and
    // detail-view never disagree on the same peer. Fall back to recomputing
    // here for older peers whose SystemUpdateOutput predates the field.
    let update_available = out
        .update_available
        .unwrap_or_else(|| match (&version, &latest) {
            (Some(v), Some(l)) => system::update_state::is_update_available(v, l),
            _ => false,
        });

    let now = utils::time::now().unix_seconds();
    let row = db::peer_update_state::PeerUpdateState {
        peer_id: peer_id.to_string(),
        version,
        channel,
        pinned_to: out.pinned_to,
        latest,
        update_available,
        checked_at: Some(now),
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_default()?;
        db::peer_update_state::upsert(&conn, &row)?;
        Ok(())
    })
    .await??;
    Ok(())
}
