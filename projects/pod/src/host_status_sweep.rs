//! Periodic enforcement of per-peer retention caps.
//!
//! Walks every peer in `host_status` and applies the resolved per-peer
//! policy (age + size + count). Runs every [`SWEEP_INTERVAL`] — caps are
//! "eventually enforced", not safety-critical. Idempotent.
//!
//! Per `feedback_retention_is_per_system_not_global`: this is the single
//! enforcer for the per-peer contract. JSONL ring honor for per-peer caps
//! is a separate task (see `system_info::history`).

use std::sync::OnceLock;
use std::time::Duration;

/// How often the sweeper wakes. Tight enough that operator changes
/// take effect quickly; loose enough that it doesn't compete with the
/// per-tick writer for the connection.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Spawn the periodic sweeper. Idempotent — second call is a no-op.
pub fn spawn() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let shutdown = utils::shutdown::token();
        // Initial delay: let startup probes settle so the first sweep
        // doesn't contend with peer pairing inserts.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = shutdown.cancelled() => return,
        }
        loop {
            if let Err(e) = sweep_once().await {
                tracing::warn!("host_status sweep tick failed: {e:#}");
            }
            tokio::select! {
                _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
                _ = shutdown.cancelled() => return,
            }
        }
    });
}

async fn sweep_once() -> anyhow::Result<()> {
    let report = tokio::task::spawn_blocking(|| -> anyhow::Result<db::host_status::SweepReport> {
        db::pool::with_pooled_or_open(|conn| {
            let now = utils::time::now().unix_seconds();
            db::host_status::sweep_all(conn, now)
        })
    })
    .await??;

    if report.total() > 0 {
        tracing::info!(
            deleted_by_age = report.deleted_by_age,
            deleted_by_size = report.deleted_by_size,
            deleted_by_count = report.deleted_by_count,
            "host_status sweep: deleted {} rows",
            report.total()
        );
    }
    Ok(())
}
