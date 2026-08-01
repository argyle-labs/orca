//! Background task that owns this host's OWN `host_status` rows.
//!
//! `spawn_local_writer` — every cadence tick, pulls the current `system_info`
//! snapshot and writes one row to this host's own `host_status` timeseries,
//! then fans it out to in-process subscribers (UI sessions, mesh forwarder).
//!
//! This is the ONLY host_status writer. Peer status is NOT mirrored into this
//! host's DB — under the data-classification law telemetry stays local to its
//! origin and is fetched on demand (`pod::peer_info`). The old cross-mesh sync
//! puller and subscription replica have been removed; `host_status` is now this
//! host's own local, retention-capped history (see `host_status_sweep`).
//!
//! The task is idempotent: callers can fire `spawn_local_writer` more than once
//! and only the first invocation actually starts a task.

use anyhow::{Context, Result};
use std::sync::OnceLock;
use std::time::Duration;
use system::system_info_types::SystemInfoReport;

// Cadence is adaptive — see `subscribe_demand::choose_cadence`. When any UI
// session is actively subscribed the writer runs at FAST_CADENCE (~2s) so
// version / mode / channel changes surface promptly; with nobody watching it
// drops to SLOW_CADENCE (~30s).

/// History points to embed in a *persisted* host_status row. The live
/// snapshot carries `history::read_tail(720)` (~720 points ≈ 100 KB) for the
/// UI graph, but the host_status ROWS are themselves the time series — storing
/// the full ring in every row duplicates the whole history into each of the
/// thousands of rows written per day (per-row payload ~104 KB), which balloons
/// the DB to multiple GB fleet-wide. The live subscriber/API path keeps the
/// full ring; only the durable copy is capped to this recent tail.
const PERSIST_HISTORY_POINTS: usize = 60;

pub fn spawn_local_writer() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let shutdown = utils::shutdown::token();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = shutdown.cancelled() => return,
        }
        loop {
            if let Err(e) = persist_local_snapshot().await {
                tracing::warn!("host_status local writer: {e:#}");
            }
            let next = crate::subscribe_demand::choose_cadence(
                crate::subscribe_demand::is_live(),
                crate::subscribe_demand::FAST_CADENCE,
                crate::subscribe_demand::SLOW_CADENCE,
            );
            tokio::select! {
                _ = tokio::time::sleep(next) => {}
                _ = shutdown.cancelled() => return,
            }
        }
    });
}

/// Own-peer id used as the row key. `peer.<machine_id_short>` matches the
/// canonical pod-mesh identity used everywhere else.
fn own_peer_id() -> String {
    system::host_identity::machine_id().to_string()
}

async fn persist_local_snapshot() -> Result<()> {
    // Prefer the in-memory cache so cpu_usage_percent is a real delta (not the
    // first-call zero that collect_blocking() always returns). Fall back to a
    // fresh collect only when the background refresher hasn't run yet.
    let snap = if let Some(cached) = system::system_info::current() {
        (*cached).clone()
    } else {
        tokio::task::spawn_blocking(system::system_info::collect_blocking).await?
    };
    // Full payload for live subscribers (UI history graph keeps all 720 points).
    let payload = serde_json::to_string(&snap).context("serialise SystemInfoReport")?;
    // Slim payload for durable storage: cap the embedded history ring to the
    // most-recent `PERSIST_HISTORY_POINTS` so stored rows stay small. Reuse the
    // already-owned `snap` (it's a clone) by truncating in place — `payload`
    // above already captured the full ring for the live path.
    let db_payload = {
        let mut slim = snap;
        let len = slim.history.len();
        if len > PERSIST_HISTORY_POINTS {
            slim.history.drain(0..len - PERSIST_HISTORY_POINTS);
        }
        let snapshot_at = slim
            .snapshot_at_unix
            .unwrap_or_else(|| utils::time::now().unix_seconds());
        (
            serde_json::to_string(&slim).context("serialise slim SystemInfoReport")?,
            snapshot_at,
        )
    };
    let (payload_for_insert, snapshot_at) = db_payload;
    let now = utils::time::now().unix_seconds();
    let peer_id = own_peer_id();

    tokio::task::spawn_blocking(move || -> Result<()> {
        db::pool::with_pooled_or_open(|conn| {
            db::host_status::insert_status(conn, snapshot_at, &payload_for_insert, now)?;
            Ok(())
        })
    })
    .await??;
    // Invalidate the host_status cache so the next pod.list read sees the
    // fresh row instead of a stale entry from the previous tick.
    db::cache::invalidate_host_status(&peer_id);

    // Fan out to in-process subscribers (UI sessions, mesh forwarder).
    // Best-effort: failures here don't roll back the DB write.
    crate::subscribe::publish_host_status(crate::subscribe::HostStatusEvent {
        peer_id,
        snapshot_at_unix: snapshot_at,
        payload,
    });
    Ok(())
}

// Silence the unused-import warning when the file is touched in isolation.
#[allow(dead_code)]
fn _typecheck(_: SystemInfoReport) {}
