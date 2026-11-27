//! Async time for plugins. The executor stays hidden: deadlines go through
//! [`Deadline`], never a runtime `Instant`.

use std::future::Future;
use std::time::Duration;

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

pub use ::utils::time::{Deadline, Polled, Probe, poll_until};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_some_when_fast() {
        assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_none_when_slow() {
        let out = timeout(Duration::from_millis(5), async {
            sleep(Duration::from_secs(30)).await;
            7
        })
        .await;
        assert_eq!(out, None);
    }
}
