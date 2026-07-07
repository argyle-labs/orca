//! In-process pub/sub bus for owned host data.
//!
//! Slice A of the subscription protocol (`project_data_ownership_and_realtime.md`).
//! Each daemon owns its own data and broadcasts updates on a typed bus;
//! local consumers (UI WebSocket sessions, the future mesh forwarder)
//! subscribe with `tokio::sync::broadcast`.
//!
//! Lossy by design: if a slow subscriber lags by more than [`CAPACITY`]
//! events, it gets `RecvError::Lagged` and skips ahead. Subscribers that
//! need a complete history should read from `host_status` instead.

use std::sync::OnceLock;
use tokio::sync::broadcast;

/// Bounded ring buffer per bus. Sized to absorb a multi-second stall in a
/// subscriber without dropping events under steady-state cadence (≤ a few
/// per second per topic in slice A).
const CAPACITY: usize = 128;

#[derive(Debug, Clone)]
pub struct HostStatusEvent {
    /// Owner peer_id — the host that produced this snapshot.
    pub peer_id: String,
    pub snapshot_at_unix: i64,
    /// JSON-serialized `SystemInfoReport`. Kept as a string so the bus
    /// stays cheap to clone and avoids re-serializing per subscriber.
    pub payload: String,
}

fn host_status_bus() -> &'static broadcast::Sender<HostStatusEvent> {
    static BUS: OnceLock<broadcast::Sender<HostStatusEvent>> = OnceLock::new();
    BUS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(CAPACITY);
        tx
    })
}

/// Publish a host-status snapshot to all active subscribers. Returns the
/// number of subscribers that received the event (0 is fine — the bus is
/// fire-and-forget).
pub fn publish_host_status(event: HostStatusEvent) -> usize {
    publish_to(host_status_bus(), event)
}

/// Send `event` on `sender`, normalizing the "no subscribers" Err into 0.
/// Extracted for testability — we can pass in a private sender to
/// deterministically exercise the no-subscriber branch.
fn publish_to(sender: &broadcast::Sender<HostStatusEvent>, event: HostStatusEvent) -> usize {
    sender.send(event).unwrap_or(0)
}

/// Subscribe to host-status events. The returned receiver only sees events
/// published AFTER subscription; backfill must come from the DB.
pub fn subscribe_host_status() -> broadcast::Receiver<HostStatusEvent> {
    host_status_bus().subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn recv_for(
        rx: &mut broadcast::Receiver<HostStatusEvent>,
        want_peer: &str,
    ) -> HostStatusEvent {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("timed out waiting for event")
                .expect("recv ok");
            if ev.peer_id == want_peer {
                return ev;
            }
        }
    }

    #[tokio::test]
    async fn publish_never_panics_regardless_of_subscriber_count() {
        // The bus is a global, so other concurrent tests may hold subscribers.
        // We only guarantee fire-and-forget: any non-panicking return is fine.
        let _ = publish_host_status(HostStatusEvent {
            peer_id: "test-no-subs-peer".into(),
            snapshot_at_unix: 1,
            payload: "{}".into(),
        });
    }

    #[tokio::test]
    async fn publish_to_local_sender_returns_zero_when_no_subscribers() {
        // Deterministic version of the above: a fresh sender with no
        // subscribers must yield 0, exercising the Err → 0 branch.
        let (tx, _) = broadcast::channel::<HostStatusEvent>(4);
        let n = publish_to(
            &tx,
            HostStatusEvent {
                peer_id: "x".into(),
                snapshot_at_unix: 0,
                payload: String::new(),
            },
        );
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn two_subscribers_both_receive_published_event() {
        let mut rx1 = subscribe_host_status();
        let mut rx2 = subscribe_host_status();

        let n = publish_host_status(HostStatusEvent {
            peer_id: "test-roundtrip-peer".into(),
            snapshot_at_unix: 100,
            payload: "snap".into(),
        });
        assert!(n >= 2, "expected at least the two subscribers, got {n}");

        let e1 = recv_for(&mut rx1, "test-roundtrip-peer").await;
        let e2 = recv_for(&mut rx2, "test-roundtrip-peer").await;
        assert_eq!(e1.snapshot_at_unix, 100);
        assert_eq!(e1.payload, "snap");
        assert_eq!(e2.snapshot_at_unix, 100);
    }

    #[tokio::test]
    async fn late_subscriber_does_not_see_earlier_events() {
        publish_host_status(HostStatusEvent {
            peer_id: "test-late-peer".into(),
            snapshot_at_unix: 5,
            payload: "before".into(),
        });
        let mut rx = subscribe_host_status();
        // Nothing buffered for this subscriber — a short timeout should fire
        // unless someone else's test publishes "test-late-peer", which they
        // won't (unique peer_id per test).
        let res = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        if let Ok(Ok(ev)) = res {
            assert_ne!(
                ev.peer_id, "test-late-peer",
                "late subscriber should not see pre-subscription event"
            );
        }
    }
}
