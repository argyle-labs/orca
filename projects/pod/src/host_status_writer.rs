//! Background tasks that own the `host_status` table:
//!
//!   * `spawn_local_writer` — every [`PERSIST_INTERVAL`], pulls the current
//!     `system_info` snapshot and writes one row under this host's own
//!     `peer_id`. Source = `"local"`.
//!   * `spawn_sync_puller` — every [`SYNC_INTERVAL`], for every active
//!     paired peer, asks that peer for its own status rows since the local
//!     watermark and inserts them as `source="synced"`. Catches up
//!     automatically when a peer comes back online — the watermark just
//!     hasn't moved.
//!
//! Both tasks are idempotent: callers can fire `spawn_…` more than once and
//! only the first invocation actually starts a task.

use crate::status::HostStatusRows;
use anyhow::{Context, Result};
use std::sync::OnceLock;
use std::time::Duration;
use system::system::SystemStatusReport;
use system::system_info_types::SystemInfoReport;

use crate::runtime_cache;

// Cadence is adaptive — see `subscribe_demand::choose_cadence`. When any UI
// session is actively subscribed the puller runs at FAST_CADENCE (~2s) so
// version / mode / channel changes surface promptly; with nobody watching it
// drops to SLOW_CADENCE (~30s). Per-tool refresh hooks (e.g. system.update)
// still force-refresh immediately on success — this is the fleetwide
// fallback.

/// Max rows requested per peer per sync tick. Bounds catch-up work after a
/// peer reconnects from a long outage; still well under
/// `MAX_ROWS_PER_PEER` so a single tick can fully drain a fresh peer's
/// buffered rows.
const SYNC_LIMIT_PER_TICK: u32 = 512;

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

pub fn spawn_sync_puller() {
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
            if let Err(e) = pull_peer_status_once().await {
                tracing::warn!("host_status sync puller: {e:#}");
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
    let payload = serde_json::to_string(&snap).context("serialise SystemInfoReport")?;
    let snapshot_at = snap
        .snapshot_at_unix
        .unwrap_or_else(|| utils::time::now().unix_seconds());
    let now = utils::time::now().unix_seconds();
    let peer_id = own_peer_id();

    let peer_id_for_insert = peer_id.clone();
    let payload_for_insert = payload.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        db::pool::with_pooled_or_open(|conn| {
            db::host_status::insert_status(
                conn,
                &peer_id_for_insert,
                snapshot_at,
                &payload_for_insert,
                now,
                "local",
            )?;
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

async fn pull_peer_status_once() -> Result<()> {
    let peers = tokio::task::spawn_blocking(|| -> Result<Vec<(String, String)>> {
        let rows = db::pool::with_pooled_or_open(db::pod::list_peer_summaries)?;
        // (peer_id, addr) — skip departed peers, skip our own row (no point
        // pulling ourselves; the local writer owns those).
        let own = own_peer_id();
        Ok(rows
            .into_iter()
            .filter(|p| p.status == "active" && p.peer_id != own)
            .map(|p| (p.peer_id, p.addr))
            .collect())
    })
    .await??;

    let ids: Vec<String> = peers.iter().map(|(p, _)| p.clone()).collect();
    tracing::info!("host_status puller tick: {} peers — {:?}", ids.len(), ids);
    let mut handles = Vec::with_capacity(peers.len());
    for (peer_id, addr) in peers {
        handles.push(tokio::spawn(pull_one_peer(peer_id, addr)));
    }
    for h in handles {
        if let Err(e) = h.await {
            tracing::debug!("host_status puller task join error: {e:#}");
        }
    }
    Ok(())
}

async fn pull_one_peer(peer_id: String, addr: String) {
    if let Err(e) = pull_one_peer_inner(&peer_id, &addr).await {
        // Common in normal operation (peer offline, mid-restart). Trace, not
        // warn, so the log stays usable.
        tracing::debug!("host_status pull from {peer_id} ({addr}): {e:#}");
    }
}

async fn pull_one_peer_inner(peer_id: &str, addr: &str) -> Result<()> {
    // Build the multi-channel dial list off-thread (DB I/O), then race through
    // it sequentially. Falls back to the single legacy `addr` when the peer
    // hasn't yet propagated a snapshot.
    let targets = {
        let pid = peer_id.to_string();
        let addr_owned = addr.to_string();
        tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let conn = db::open_default()?;
            crate::dialer::dial_targets_for_peer(&conn, &pid, &addr_owned)
        })
        .await??
    };

    // Refresh peer_hostname + addressing opportunistically — pod/ping always
    // returns the OS hostname, and rc.25+ peers also include a full addressing
    // snapshot (display_name + per-channel addresses). Display name from the
    // snapshot wins; fall back to OS hostname for rc.≤24 peers.
    let ping_fut = crate::dialer::try_targets(&targets, |t| async move { crate::ping(&t).await });
    if let Ok(Ok(pong)) = tokio::time::timeout(Duration::from_secs(5), ping_fut).await {
        let pid = peer_id.to_string();
        let host = pong
            .addressing
            .as_ref()
            .map(|a| a.display_name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| pong.hostname.clone());
        let addressing = pong.addressing.clone();
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = db::open_default()?;
            db::pod::update_hostname(&conn, &pid, &host)?;
            if let Some(snap) = addressing {
                let entries: Vec<(&str, &str)> = snap
                    .channels
                    .iter()
                    .map(|c| (c.kind.as_str(), c.value.as_str()))
                    .collect();
                db::host_addressing::replace_peer_addresses_from_source(
                    &mut conn,
                    &pid,
                    "autodetect",
                    &entries,
                )?;
            }
            Ok(())
        })
        .await;
        match join {
            Err(e) => tracing::warn!("[pod] hostname/addressing refresh: join failed: {e:#}"),
            Ok(Err(e)) => tracing::warn!("[pod] hostname/addressing refresh failed: {e:#}"),
            Ok(Ok(())) => {}
        }
    }

    let watermark = {
        let pid = peer_id.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<i64>> {
            let conn = db::open_default()?;
            db::host_status::latest_snapshot_at(&conn, &pid)
        })
        .await??
    };

    let args = serde_json::json!({
        "peer_id": peer_id,
        "since_unix": watermark,
        "limit": SYNC_LIMIT_PER_TICK,
    });
    let exec_res = tokio::time::timeout(
        Duration::from_secs(15),
        crate::exec(addr, "pod.history", args),
    )
    .await
    .context("pod/exec timeout")??;

    // Best-effort runtime-cache refresh — see `refresh_runtime_from_addr`.
    _ = refresh_runtime_from_addr(peer_id, addr).await;

    let rows: HostStatusRows =
        serde_json::from_value(exec_res.result).context("decode host_status.detail response")?;

    // SAFETY: the peer is authoritative for *its own* peer_id only. Drop any
    // row whose peer_id doesn't match what we asked for; that protects this
    // host's DB from a misbehaving (or compromised) peer trying to inject
    // status for someone else.
    let now = utils::time::now().unix_seconds();
    let owner_peer_id = peer_id.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_default()?;
        for row in rows.0 {
            if row.peer_id != owner_peer_id {
                continue;
            }
            let payload = match row.system {
                Some(s) => serde_json::to_string(&s).unwrap_or_default(),
                None => continue, // skip rows we can't decode
            };
            _ = db::host_status::insert_status(
                &conn,
                &owner_peer_id,
                row.snapshot_at_unix,
                &payload,
                now,
                "synced",
            );
        }
        Ok(())
    })
    .await??;
    Ok(())
}

/// Best-effort: fetch `system.detail` from a peer at `addr` and stash the
/// runtime fields (version / target / frontend / mode / channel / pinned_to)
/// into the in-memory `runtime_cache`. Returns `Err` if the call fails so
/// callers can retry; the puller path ignores the result because its next
/// tick will try again anyway.
pub async fn refresh_runtime_from_addr(peer_id: &str, addr: &str) -> Result<()> {
    let detail_res = tokio::time::timeout(
        Duration::from_secs(5),
        crate::exec(addr, "system.detail", serde_json::json!({})),
    )
    .await
    .context("system.detail timeout")??;
    let detail: SystemStatusReport =
        serde_json::from_value(detail_res.result).context("decode system.detail response")?;
    runtime_cache::put(
        peer_id,
        runtime_cache::RuntimeFields {
            version: Some(detail.version),
            target: Some(detail.target),
            frontend: Some(detail.frontend),
            mode: detail.mode,
            channel: detail.channel,
            pinned_to: detail.pinned_to,
        },
    );
    Ok(())
}

/// Resolve a peer_id to its dial addr via the local pod_peers row, then
/// refresh its runtime snapshot. Used after `system.update --peer <h>`
/// completes so the UI reflects the new version immediately rather than
/// waiting up to one sync tick.
pub async fn refresh_runtime_for_peer(peer_id: &str) -> Result<()> {
    let pid = peer_id.to_string();
    let addr = tokio::task::spawn_blocking(move || -> Result<String> {
        let conn = db::open_default()?;
        let peers = db::pod::list_peer_summaries(&conn)?;
        peers
            .into_iter()
            .find(|p| p.peer_id == pid)
            .map(|p| p.addr)
            .ok_or_else(|| anyhow::anyhow!("peer {pid} not in pod_peers"))
    })
    .await??;
    refresh_runtime_from_addr(peer_id, &addr).await
}

// Silence the unused-import warning when the file is touched in isolation.
#[allow(dead_code)]
fn _typecheck(_: SystemInfoReport) {}
