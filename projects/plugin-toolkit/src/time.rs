//! Async time — an orca abstraction, not a runtime re-export.
//!
//! Plugins stay `async` but reach timing through this orca-owned surface, so no
//! executor type or crate name ever appears in plugin code. The executor behind
//! these functions (tokio today) is an orca implementation detail: swap it and
//! no plugin changes. Per [[orca-north-star-abstract-system-differences]] and
//! [[plugins-stay-thin]] — the plugin depends on `plugin_toolkit`, never on the
//! runtime.
//!
//! `Duration` is std vocabulary (like `String`), not a runtime dependency, so it
//! is used directly at the boundary; every other type here is orca-owned. There
//! is deliberately no way to obtain the executor's clock/`Instant` — use
//! [`Deadline`].

use std::future::Future;
use std::time::{Duration, Instant};

// Wall-clock time (an orca-owned instant, chrono hidden) is the `utils::time`
// utility. Re-exposed here so a plugin reaches both async time and wall-clock
// time through one `time` module and never names the datetime library. Gated on
// `tools` (which pulls light, tree-shaken `utils` — no http/tls/git), so any
// tool plugin has it.
#[cfg(feature = "tools")]
pub use ::utils::time::{Timestamp, now};

/// Suspend the current task for `dur` without blocking a thread.
pub async fn sleep(dur: Duration) {
    tokio::time::sleep(dur).await
}

/// Await `fut` with a deadline. `Some(output)` if it finished within `dur`,
/// `None` if it timed out. The executor's timeout-error type never surfaces.
pub async fn timeout<F: Future>(dur: Duration, fut: F) -> Option<F::Output> {
    tokio::time::timeout(dur, fut).await.ok()
}

/// Spawn a fire-and-forget background task on the runtime. The task runs to
/// completion independently; there is no handle to await (use for best-effort
/// background work like cache refresh). orca-owned so a plugin backgrounds work
/// without naming the executor's `spawn`.
pub fn spawn_detached<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    drop(tokio::spawn(fut));
}

/// Drive `fut` to completion and return its output — for synchronous bridges
/// and plugin tests, so they await async code without naming the runtime. Must
/// not be called from inside a running executor (it owns its own).
pub fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime")
        .block_on(fut)
}

/// A monotonic point in the future. orca-owned so plugins express "have we run
/// past a budget?" without naming a runtime clock. Construct with
/// [`Deadline::after`], poll with [`Deadline::reached`].
#[derive(Clone, Copy, Debug)]
pub struct Deadline(Instant);

impl Deadline {
    /// A deadline `dur` from now.
    pub fn after(dur: Duration) -> Self {
        Self(Instant::now() + dur)
    }

    /// True once the current time is at or past the deadline.
    pub fn reached(&self) -> bool {
        Instant::now() >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_some_when_fast() {
        assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn timeout_none_when_slow() {
        let out = timeout(Duration::from_millis(5), async {
            sleep(Duration::from_secs(30)).await;
            7
        })
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn deadline_reached_after_elapse() {
        let d = Deadline::after(Duration::from_millis(5));
        assert!(!d.reached());
        sleep(Duration::from_millis(15)).await;
        assert!(d.reached());
    }
}
