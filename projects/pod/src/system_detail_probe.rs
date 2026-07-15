//! Periodic per-peer `system.detail {}` probe.
//!
//! Every 120s, for each active paired peer, this task issues a READ-ONLY
//! `system.detail {}` over the pod mesh and persists the full result JSON in
//! `peer_detail_state`. `pod.list` joins that table when building peer
//! entries so the web UI's per-peer drawer is already hydrated when the
//! user opens it — no on-open RPC needed for remote peers.
//!
//! ── SAFETY ────────────────────────────────────────────────────────────────
//! This probe MUST send `{}` and nothing else. `system.detail` is read-only,
//! but the policy of "periodic probes never carry args" mirrors
//! `update_state_probe` so a future schema change can't turn a probe into a
//! mutation by accident. If you need to mutate peer state, do it from the
//! user-facing tool path with an explicit caller identity, not from this
//! loop.
//!
//! Failures are logged at debug and DO NOT poison the row — the last good
//! payload stays in place; only `checked_at` advances on success.

use anyhow::{Context, Result};
use std::time::Duration;
use system::periodic::{PeriodicSpec, spawn};

/// Cadence between probe ticks. Slower than `update_state_probe` (60s) —
/// `system.detail` is heavier (storage walk, diagnostic collection,
/// host_addressing query) and the fields it carries (CPU/mem/etc.) churn
/// fast enough at the SystemInfoReport level that we already have the
/// `host_status` mirror catching short-term drift. 120s is plenty for
/// drawer-hydration freshness.
const PROBE_INTERVAL_SECS: u64 = 120;

/// Per-peer call budget. `system.detail` does several local lookups before
/// it can answer (install_status, diagnostic, channels) so the budget is the
/// same as the update probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

pub fn spawn_periodic() {
    use std::sync::OnceLock;
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::mem::drop(spawn(
        PeriodicSpec {
            name: "pod.peer_detail_probe.tick",
            initial_delay: Duration::from_secs(15),
            interval: Duration::from_secs(PROBE_INTERVAL_SECS),
        },
        system::periodic::boxed(probe_tick),
    ));
}

async fn probe_tick() -> Result<()> {
    let peers = tokio::task::spawn_blocking(|| -> Result<Vec<(String, String)>> {
        let conn = db::open_default()?;
        let rows = db::pod::list_peer_summaries(&conn)?;
        let own = system::host_identity::machine_id().to_string();
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
            tracing::debug!("peer_detail_probe join: {e:#}");
        }
    }
    Ok(())
}

async fn probe_one(peer_id: String, addr: String) {
    match tokio::time::timeout(PROBE_TIMEOUT, probe_one_inner(&peer_id, &addr)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!("peer_detail_probe {peer_id} ({addr}): {e:#}"),
        Err(_) => tracing::debug!("peer_detail_probe {peer_id} ({addr}): timeout"),
    }
}

async fn probe_one_inner(peer_id: &str, addr: &str) -> Result<()> {
    // READ-ONLY probe — `{}` only. See module-level SAFETY note.
    let res = crate::exec(addr, "system.detail", serde_json::json!({})).await?;
    let payload = serde_json::to_string(&res.result).context("encode system.detail payload")?;
    let now = utils::time::now().unix_seconds();
    let row = db::peer_detail_state::PeerDetailState {
        peer_id: peer_id.to_string(),
        payload,
        checked_at: now,
    };
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_default()?;
        db::peer_detail_state::upsert(&conn, &row)?;
        Ok(())
    })
    .await??;
    Ok(())
}
