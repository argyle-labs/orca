//! Reusable periodic-loop primitive.
//!
//! Three production tickers (pod auto-offer, cert rotation, host-identity
//! refresh) historically each hand-rolled the same `loop { tick().await;
//! sleep(interval).await }` shape. This module is the shared scaffold and
//! adds free observability — every tick is recorded to `scheduler_runs`
//! with outcome and duration.
//!
//! Single-instance is implicit: the daemon owns the handle, runs one
//! process per host. No locking, no leader election.

use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::debug;

pub use utils::shutdown::{shutdown, token as shutdown_token};

/// A periodic job's logic. Returned errors are logged at `debug` and
/// recorded in `scheduler_runs`; the loop keeps running.
pub type TickFn = Box<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Configure a periodic job.
pub struct PeriodicSpec {
    /// Canonical job name (e.g. `pod.scheduler.run`, `host.backup.run`).
    /// Used as the `scheduler_runs.job_name` key.
    pub name: &'static str,
    /// Delay before the first tick. Useful for "don't slam the daemon at
    /// startup" — set to a few seconds (or longer for daily jobs).
    pub initial_delay: Duration,
    /// Interval between tick start times (sleep-after-tick semantics —
    /// long ticks shift the next start, no overlap).
    pub interval: Duration,
}

/// Spawn the loop. Returns immediately; the task runs until the process
/// exits. Callers should drop the handle to "leak it" — that's the
/// daemon-task convention.
pub fn spawn(spec: PeriodicSpec, tick: TickFn) -> JoinHandle<()> {
    tokio::spawn(async move {
        let shutdown = shutdown_token();
        if !spec.initial_delay.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(spec.initial_delay) => {}
                _ = shutdown.cancelled() => return,
            }
        }
        loop {
            run_one(spec.name, &tick).await;
            tokio::select! {
                _ = tokio::time::sleep(spec.interval) => {}
                _ = shutdown.cancelled() => return,
            }
        }
    })
}

async fn run_one(name: &'static str, tick: &TickFn) {
    let started = Utc::now();
    let started_iso = started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let t0 = std::time::Instant::now();

    let outcome = tick().await;

    let finished_iso = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let duration_ms = t0.elapsed().as_millis() as i64;
    let (ok, error) = match &outcome {
        Ok(()) => (true, None),
        Err(e) => (false, Some(format!("{e:#}"))),
    };

    if let Err(e) = &outcome {
        debug!("[{name}] tick: {e:#}");
    }

    // Record run history. Failures here should not break the loop —
    // observability is best-effort.
    if let Err(e) = record_run(
        name,
        &started_iso,
        &finished_iso,
        ok,
        error.as_deref(),
        duration_ms,
    ) {
        debug!("[{name}] record_run: {e:#}");
    }
}

fn record_run(
    name: &str,
    started_at: &str,
    finished_at: &str,
    ok: bool,
    error: Option<&str>,
    duration_ms: i64,
) -> anyhow::Result<()> {
    let conn = db::open_default()?;
    db::scheduler_runs::record(&conn, name, started_at, finished_at, ok, error, duration_ms)?;
    Ok(())
}

/// Convenience: wrap an async fn into a `TickFn`.
///
/// ```ignore
/// periodic::spawn(spec, periodic::boxed(my_tick));
/// ```
pub fn boxed<F, Fut>(f: F) -> TickFn
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    Box::new(move || Box::pin(f()))
}
