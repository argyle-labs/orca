//! Process-wide cooperative shutdown.
//!
//! Background loops (periodic tickers, pull/push replicators, host-status
//! writers, the mesh accept loop) honor a global [`CancellationToken`] so
//! daemon shutdown drains them cleanly instead of letting the Tokio runtime
//! abort them mid-await on drop.
//!
//! Two primitives, one global of each:
//!
//!   * [`token`] — a **sticky** [`CancellationToken`]. Loops `select!` on
//!     `token().cancelled()` against their sleep/recv future. Sticky means a
//!     task that subscribes *after* [`shutdown`] has fired still observes the
//!     cancellation immediately — the bare `Notify` it replaced only woke
//!     tasks already parked in `notified()`, so a task spawned during the
//!     shutdown window could miss the signal and run forever.
//!   * [`tracker`] — a [`TaskTracker`] for in-flight work that must *finish*,
//!     not just be cancelled (e.g. a peer tool-call mid-flight). Register
//!     such work with `tracker().spawn(...)` / `tracker().track_future(...)`;
//!     [`drain`] waits for all tracked tasks to complete under a timeout.
//!
//! [`shutdown`] is idempotent — called from every daemon exit branch.

use std::sync::OnceLock;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// The process-wide sticky cancellation token. Cancellation is permanent and
/// observable by tasks that subscribe before or after it fires.
pub fn token() -> &'static CancellationToken {
    static TOKEN: OnceLock<CancellationToken> = OnceLock::new();
    TOKEN.get_or_init(CancellationToken::new)
}

/// The process-wide task tracker for drain-on-shutdown work. Tasks registered
/// here are awaited by [`drain`] so in-flight operations finish before the
/// daemon exits.
pub fn tracker() -> &'static TaskTracker {
    static TRACKER: OnceLock<TaskTracker> = OnceLock::new();
    TRACKER.get_or_init(TaskTracker::new)
}

/// Signal shutdown: cancel the token and close the tracker so [`drain`] can
/// complete. Idempotent — safe to call from every exit branch.
pub fn shutdown() {
    token().cancel();
    // Closing the tracker lets `wait()` return once all *currently tracked*
    // tasks finish. New tasks must not be registered after this point;
    // `TaskTracker::spawn` on a closed tracker still runs them but they are
    // not awaited by an already-resolved `wait()`.
    tracker().close();
}

/// Whether shutdown has been requested. Cheap, lock-free.
pub fn is_shutting_down() -> bool {
    token().is_cancelled()
}

/// Cancel + close, then wait for all tracked tasks to drain, bounded by
/// `timeout`. Returns `true` if every tracked task finished within the
/// budget, `false` if the timeout fired first (caller should force-exit).
///
/// Calls [`shutdown`] itself, so callers don't need to cancel separately.
pub async fn drain(timeout: Duration) -> bool {
    shutdown();
    tokio::time::timeout(timeout, tracker().wait())
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracked_task_holding_a_write_completes_before_drain_returns() {
        // A cooperative loop that holds a "write" (a mutex guard) and must be
        // allowed to finish that critical section before drain returns.
        let token = CancellationToken::new();
        let tracker = TaskTracker::new();
        let data = std::sync::Arc::new(tokio::sync::Mutex::new(0u32));

        let child_token = token.clone();
        let child_data = data.clone();
        tracker.spawn(async move {
            // Simulate a write in progress that must complete atomically.
            let mut guard = child_data.lock().await;
            tokio::select! {
                _ = child_token.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            *guard = 42; // critical section finishes regardless of cancel
        });

        token.cancel();
        tracker.close();
        // Wait must not return until the write completed.
        let drained = tokio::time::timeout(Duration::from_secs(2), tracker.wait())
            .await
            .is_ok();
        assert!(drained, "tracker.wait() should complete within timeout");
        assert_eq!(*data.lock().await, 42, "write must have completed");
    }

    #[tokio::test]
    async fn drain_times_out_when_task_never_finishes() {
        let tracker = TaskTracker::new();
        tracker.spawn(async {
            // Ignores cancellation entirely — drain must time out.
            std::future::pending::<()>().await;
        });
        tracker.close();
        let drained = tokio::time::timeout(Duration::from_millis(100), tracker.wait())
            .await
            .is_ok();
        assert!(!drained, "wait() must not complete for a never-ending task");
    }

    #[tokio::test]
    async fn token_cancellation_is_sticky_for_late_subscribers() {
        let token = CancellationToken::new();
        token.cancel();
        // Subscribing AFTER cancellation must observe it immediately.
        tokio::time::timeout(Duration::from_millis(100), token.cancelled())
            .await
            .expect("late subscriber sees sticky cancellation");
    }
}
