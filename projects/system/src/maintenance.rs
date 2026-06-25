//! Periodic housekeeping for unbounded on-disk state.
//!
//! Two leaks share one slow cadence here:
//!
//!   * `update::prune_check_cache` — drops `--check`-cached sha256 blobs past
//!     their 14-day TTL. Previously only ever pruned lazily on an `orca
//!     update --check` invocation, so a daemon that never ran `--check`
//!     accreted blobs forever.
//!   * `db::maintenance::sweep_session_events` — deletes `session_events`
//!     older than the retention window so the FTS-mirrored audit log can't
//!     grow without bound on a long-lived daemon.
//!
//! Both are best-effort: a failure logs at debug and the loop keeps running.
//! Hourly is plenty — these are slow-growing accretions, not hot paths.

use std::time::Duration;

use crate::periodic::{self, PeriodicSpec};

/// Cadence between maintenance sweeps. Hourly — the accretions this guards
/// against grow over days, so a tighter cadence would only burn I/O.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Retention window (days) for `session_events`. Matches the `db.sweep` tool
/// default so the periodic sweep and the operator-driven one agree.
const SESSION_EVENTS_RETENTION_DAYS: u32 = 14;

/// Register the maintenance loop on the periodic scheduler. Idempotent.
pub fn spawn_periodic() {
    use std::sync::OnceLock;
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::mem::drop(periodic::spawn(
        PeriodicSpec {
            name: "system.maintenance.sweep",
            // Stagger off the startup burst; the first sweep can wait a minute.
            initial_delay: Duration::from_secs(60),
            interval: SWEEP_INTERVAL,
        },
        periodic::boxed(sweep_tick),
    ));
}

async fn sweep_tick() -> anyhow::Result<()> {
    // Filesystem + DB work — run off the async reactor.
    tokio::task::spawn_blocking(|| {
        crate::update::prune_check_cache();
        match db::open_default() {
            Ok(conn) => {
                if let Err(e) =
                    db::maintenance::sweep_session_events(&conn, SESSION_EVENTS_RETENTION_DAYS)
                {
                    tracing::debug!("[maintenance] sweep_session_events: {e:#}");
                }
            }
            Err(e) => tracing::debug!("[maintenance] db open for sweep failed: {e:#}"),
        }
    })
    .await?;
    Ok(())
}
