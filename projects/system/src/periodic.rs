//! Reusable periodic-loop primitive.
//!
//! Three production tickers (mesh cache GC, cert rotation, host-identity
//! refresh) historically each hand-rolled the same `loop { tick().await;
//! sleep(interval).await }` shape. This module is the shared scaffold and
//! adds free observability — every tick is recorded to `scheduler_runs`
//! with outcome and duration.
//!
//! Single-instance is implicit: the daemon owns the handle, runs one
//! process per host. No locking, no leader election.

use std::time::Duration;

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
    /// Canonical job name (e.g. `mesh.scheduler.run`, `host.backup.run`).
    /// Used as the `scheduler_runs.job_name` key.
    pub name: &'static str,
    /// Delay before the first tick. Useful for "don't slam the daemon at
    /// startup" — set to a few seconds (or longer for daily jobs).
    pub initial_delay: Duration,
    /// Interval between tick start times (sleep-after-tick semantics —
    /// long ticks shift the next start, no overlap).
    pub interval: Duration,
}

static DAEMON_KEY: std::sync::OnceLock<contract::invocation_scope::DaemonKey> =
    std::sync::OnceLock::new();

pub fn init() -> Result<(), contract::invocation_scope::KeyStolen> {
    if DAEMON_KEY.get().is_some() {
        return Ok(());
    }
    let key = contract::invocation_scope::DaemonKey::claim()
        .ok_or(contract::invocation_scope::KeyStolen("daemon"))?;
    drop(DAEMON_KEY.set(key));
    Ok(())
}

pub(crate) fn daemon_key() -> Option<&'static contract::invocation_scope::DaemonKey> {
    let key = DAEMON_KEY.get();
    if key.is_none() {
        tracing::error!("daemon key not initialized; refusing to start daemon task");
    }
    key
}

/// Spawn the loop. Returns immediately; the task runs until the process
/// exits. Callers should drop the handle to "leak it" — that's the
/// daemon-task convention.
pub fn spawn(spec: PeriodicSpec, tick: TickFn) -> JoinHandle<()> {
    let Some(key) = daemon_key() else {
        return tokio::spawn(async {});
    };
    tokio::spawn(key.scope(async move {
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
    }))
}

async fn run_one(name: &'static str, tick: &TickFn) {
    let started_iso = utils::time::now_rfc3339();
    let t0 = std::time::Instant::now();

    let outcome = tick().await;

    let finished_iso = utils::time::now_rfc3339();
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
    db::pool::with_pooled_or_open(|conn| {
        db::scheduler_runs::record(conn, name, started_at, finished_at, ok, error, duration_ms)
    })?;
    Ok(())
}

/// Convenience: wrap an async fn into a `TickFn`.
///
/// ```
/// # use system::periodic::{self, PeriodicSpec};
/// # async fn my_tick() -> anyhow::Result<()> { Ok(()) }
/// # fn start(spec: PeriodicSpec) {
/// periodic::spawn(spec, periodic::boxed(my_tick));
/// # }
/// ```
pub fn boxed<F, Fut>(f: F) -> TickFn
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    Box::new(move || Box::pin(f()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn ticks_run_as_daemon_tasks() {
        init().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn(
            PeriodicSpec {
                name: "daemon-scope-test",
                initial_delay: Duration::ZERO,
                interval: Duration::from_secs(3600),
            },
            boxed(move || {
                let tx = tx.clone();
                async move {
                    tx.send(contract::invocation_scope::in_daemon_task()).ok();
                    Ok(())
                }
            }),
        );
        assert_eq!(rx.recv().await, Some(true));
        assert!(!contract::invocation_scope::in_daemon_task());
        handle.abort();
    }
}
