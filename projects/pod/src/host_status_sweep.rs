//! Periodic enforcement of this host's `host_status` retention caps.
//!
//! Applies the resolved policy (age + size + count) to this host's own rows.
//! Runs every [`SWEEP_INTERVAL`] — caps are "eventually enforced", not
//! safety-critical. Idempotent.
//!
//! This is the single enforcer for the DB retention contract. JSONL ring
//! honor for the same caps is a separate task (see `system_info::history`).

use std::sync::OnceLock;
use std::time::Duration;

/// How often the sweeper wakes. Tight enough that operator changes
/// take effect quickly; loose enough that it doesn't compete with the
/// per-tick writer for the connection.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Initial delay: let startup probes settle so the first sweep doesn't
/// contend with peer pairing inserts.
const SWEEP_INITIAL_DELAY: Duration = Duration::from_secs(30);

/// Spawn the periodic sweeper. Idempotent — second call is a no-op.
///
/// Runs on the shared [`system::periodic`] scaffold (shutdown handling + a
/// `scheduler_runs` history row per tick) instead of a hand-rolled
/// `loop { … sleep … }`. At a 5-minute cadence the per-tick recording is
/// cheap and gives operators sweep visibility via `schedule status`.
pub fn spawn() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    drop(system::periodic::spawn(
        system::periodic::PeriodicSpec {
            name: "pod.host_status.sweep",
            initial_delay: SWEEP_INITIAL_DELAY,
            interval: SWEEP_INTERVAL,
        },
        system::periodic::boxed(sweep_once),
    ));
}

async fn sweep_once() -> anyhow::Result<()> {
    let report = tokio::task::spawn_blocking(|| -> anyhow::Result<db::host_status::SweepReport> {
        db::pool::with_pooled_or_open(|conn| {
            let now = utils::time::now().unix_seconds();
            db::host_status::sweep(conn, now)
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
