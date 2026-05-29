//! Client side of `pod/subscribe` (slice C).
//!
//! Two layers:
//!   * [`dial_subscribe_host_status`] — open a fresh mTLS pod connection,
//!     run [`subscribe_wire::run_client`] over it, forwarding events into
//!     `tx`. One-shot: returns when the stream ends or errors.
//!   * [`subscribe_with_reconnect`] — pure orchestration loop: invoke a
//!     dialer fn, sleep on failure with backoff, retry until cancellation.
//!     Generic over the dialer so it's testable without TLS or DB.
//!
//! The reconnect loop is the testable surface; the TLS dial it wraps is a
//! thin shim over [`super::connect_pod_tls`] + [`subscribe_wire::run_client`].

use anyhow::Result;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

use super::subscribe::HostStatusEvent;
use super::subscribe_wire;

/// Open an mTLS pod connection to `host` and run the subscribe protocol
/// for `host:<topic_peer_id>:status`, forwarding events into `tx`. Returns
/// when the stream ends (graceful or error).
pub async fn dial_subscribe_host_status(
    host: &str,
    topic_peer_id: &str,
    tx: mpsc::Sender<HostStatusEvent>,
) -> Result<()> {
    let tls = super::connect_pod_tls(host).await?;
    subscribe_wire::run_client(tls, topic_peer_id, tx).await
}

/// Backoff schedule for the reconnect loop. Capped so a long-dead peer
/// doesn't burn CPU but reconnects within ≤30s once it returns.
fn next_backoff(prev: Duration) -> Duration {
    const MAX: Duration = Duration::from_secs(30);
    let doubled = prev.saturating_mul(2);
    if doubled > MAX { MAX } else { doubled }
}

/// Should-continue signal for the reconnect loop. Implementations let tests
/// stop the loop deterministically and let the daemon run it forever.
pub trait LoopCondition: Send {
    fn should_continue(&mut self) -> bool;
}

/// Always-true condition. Used by the daemon's long-lived task.
pub struct Forever;
impl LoopCondition for Forever {
    fn should_continue(&mut self) -> bool {
        true
    }
}

/// Run a subscription against `host` with retry-on-error. Backoff doubles
/// from 1s up to 30s; resets to 1s on a successful dial (so a flap doesn't
/// poison the cadence). Stops when `condition.should_continue()` returns
/// false OR `tx` is closed (consumer dropped).
///
/// `dialer` is injected so tests can swap in a fake without TLS or DB.
pub async fn subscribe_with_reconnect<D, Fut, C>(
    host: String,
    topic_peer_id: String,
    tx: mpsc::Sender<HostStatusEvent>,
    mut dialer: D,
    mut condition: C,
    initial_backoff: Duration,
) -> ReconnectStats
where
    D: FnMut(String, String, mpsc::Sender<HostStatusEvent>) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send,
    C: LoopCondition,
{
    let mut stats = ReconnectStats::default();
    let mut backoff = initial_backoff;
    while condition.should_continue() {
        if tx.is_closed() {
            stats.exited_on_tx_closed = true;
            return stats;
        }
        match dialer(host.clone(), topic_peer_id.clone(), tx.clone()).await {
            Ok(()) => {
                stats.successful_dials += 1;
                backoff = initial_backoff;
            }
            Err(_) => {
                stats.failed_dials += 1;
                backoff = next_backoff(backoff);
            }
        }
        if !condition.should_continue() {
            break;
        }
        tokio::time::sleep(backoff).await;
    }
    stats
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconnectStats {
    pub successful_dials: u32,
    pub failed_dials: u32,
    pub exited_on_tx_closed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// LoopCondition that returns true exactly N times then false.
    struct Counted {
        remaining: u32,
    }
    impl LoopCondition for Counted {
        fn should_continue(&mut self) -> bool {
            if self.remaining == 0 {
                false
            } else {
                self.remaining -= 1;
                true
            }
        }
    }

    #[test]
    fn next_backoff_doubles_until_cap() {
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(
            next_backoff(Duration::from_secs(20)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(99)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn forever_condition_always_true() {
        let mut f = Forever;
        assert!(f.should_continue());
        assert!(f.should_continue());
    }

    #[tokio::test]
    async fn reconnect_resets_backoff_after_success() {
        // Dial alternates: fail, ok, fail. Should record stats and reset
        // backoff to initial after the ok.
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let dialer = move |_host: String, _topic: String, _tx: mpsc::Sender<HostStatusEvent>| {
            let a = attempts_clone.clone();
            async move {
                let n = a.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("simulated dial failure {n}"))
                }
            }
        };
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(4);
        let stats = subscribe_with_reconnect(
            "host".into(),
            "peer".into(),
            tx,
            dialer,
            // Each loop iteration calls should_continue twice (top of while
            // + mid-loop early-exit check), so 5 ticks ⇒ 3 dials attempted.
            Counted { remaining: 5 },
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(stats.successful_dials, 1);
        assert_eq!(stats.failed_dials, 2);
        assert!(!stats.exited_on_tx_closed);
    }

    #[tokio::test]
    async fn reconnect_exits_when_tx_closed_before_first_dial() {
        let (tx, rx) = mpsc::channel::<HostStatusEvent>(1);
        drop(rx);
        let dialer = |_h, _t, _tx| async move { Ok(()) };
        let stats = subscribe_with_reconnect(
            "h".into(),
            "p".into(),
            tx,
            dialer,
            Forever,
            Duration::from_millis(1),
        )
        .await;
        assert!(stats.exited_on_tx_closed);
        assert_eq!(stats.successful_dials, 0);
        assert_eq!(stats.failed_dials, 0);
    }

    #[tokio::test]
    async fn reconnect_stops_when_condition_returns_false() {
        let (tx, _rx) = mpsc::channel::<HostStatusEvent>(1);
        let dialer = |_h, _t, _tx| async move { Ok(()) };
        let stats = subscribe_with_reconnect(
            "h".into(),
            "p".into(),
            tx,
            dialer,
            Counted { remaining: 0 },
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(stats.successful_dials, 0);
        assert_eq!(stats.failed_dials, 0);
    }
}
