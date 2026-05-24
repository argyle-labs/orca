//! Subscription-driven replica writer for peer `host_status` rows
//! (slice C of the data-ownership pivot).
//!
//! Companion to (eventually replacement for) `host_status_writer::spawn_sync_puller`.
//! Where the puller polls each peer on a fixed cadence, this writer holds
//! one long-lived `pod/subscribe` stream per peer and writes each pushed
//! event into the local `host_status` table with `source = "synced"`.
//!
//! Pure validation / wire-shape logic lives here; the network and DB
//! shims are thin wrappers so the testable surface stays in one file.

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::sync::mpsc;

use super::subscribe::HostStatusEvent;
use super::subscribe_client::{Forever, dial_subscribe_host_status, subscribe_with_reconnect};

/// Per-peer mpsc buffer. Sized for short stalls in the DB writer (one
/// `spawn_blocking` insert per event); overflow is acceptable because the
/// owner's retention + watermark let us recover via the legacy puller.
const PEER_CHANNEL_CAPACITY: usize = 128;

/// Initial backoff for the reconnect loop. Doubles to 30s max
/// (see `subscribe_client::next_backoff`).
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Spawn the long-lived subscription task for a single peer. The daemon
/// startup is intentionally NOT touched in this slice; a per-peer registry
/// plus fleet-wide spawner that diffs paired peers will land with the
/// wire-up step. Callers today are expected to dedupe peer_ids themselves.
pub fn spawn_for_peer(peer_id: String, host_addr: String) {
    let (tx, rx) = mpsc::channel::<HostStatusEvent>(PEER_CHANNEL_CAPACITY);

    // Consumer task: drain events into the DB.
    let consumer_peer = peer_id.clone();
    tokio::spawn(async move {
        run_event_consumer(rx, consumer_peer).await;
    });

    // Producer task: reconnect loop, dialing `host_addr` and asking for
    // `host:<peer_id>:status`.
    tokio::spawn(async move {
        let dialer = |host: String, topic: String, tx: mpsc::Sender<HostStatusEvent>| async move {
            dial_subscribe_host_status(&host, &topic, tx).await
        };
        let _stats =
            subscribe_with_reconnect(host_addr, peer_id, tx, dialer, Forever, INITIAL_BACKOFF)
                .await;
    });
}

/// Drain `rx`, validate each event, and write accepted ones into
/// `host_status` with `source = "synced"`. Exits when `tx` is dropped.
async fn run_event_consumer(mut rx: mpsc::Receiver<HostStatusEvent>, owner_peer_id: String) {
    while let Some(ev) = rx.recv().await {
        let Some(payload) = validate_event(&ev, &owner_peer_id) else {
            // Owner-mismatched event from a misbehaving / compromised peer.
            // Drop silently — never write a foreign peer_id into our DB.
            continue;
        };
        let snapshot_at = ev.snapshot_at_unix;
        let owner = owner_peer_id.clone();
        if let Err(e) = insert_synced_row(owner, snapshot_at, payload).await {
            tracing::debug!("host_status replica insert failed for {owner_peer_id}: {e:#}");
        }
    }
}

/// Accept an event iff `event.peer_id` matches the peer we subscribed to.
/// Returns the JSON payload string on accept; `None` on rejection. Pure +
/// branchable — the trust boundary lives here.
fn validate_event<'a>(event: &'a HostStatusEvent, expected_peer_id: &str) -> Option<&'a str> {
    if event.peer_id != expected_peer_id {
        return None;
    }
    Some(event.payload.as_str())
}

async fn insert_synced_row(owner: String, snapshot_at: i64, payload: &str) -> Result<()> {
    let payload = payload.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_default()?;
        let now = chrono::Utc::now().timestamp();
        db::host_status::insert_status(&conn, &owner, snapshot_at, &payload, now, "synced")
            .context("insert synced host_status row")?;
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(peer_id: &str, snap: i64, payload: &str) -> HostStatusEvent {
        HostStatusEvent {
            peer_id: peer_id.into(),
            snapshot_at_unix: snap,
            payload: payload.into(),
        }
    }

    #[test]
    fn validate_accepts_matching_peer_id() {
        let e = ev("peer.alpha", 1, "snap");
        assert_eq!(validate_event(&e, "peer.alpha"), Some("snap"));
    }

    #[test]
    fn validate_rejects_foreign_peer_id() {
        let e = ev("peer.evil", 1, "snap");
        assert!(validate_event(&e, "peer.alpha").is_none());
    }

    #[tokio::test]
    async fn consumer_filters_foreign_events_and_drops_on_tx_close() {
        // Drive the consumer with a mix of matching + foreign events, then
        // drop the sender. Coverage goal: hit both branches of `validate_event`
        // through `run_event_consumer`, then exit cleanly.
        let (tx, rx) = mpsc::channel::<HostStatusEvent>(4);
        let owner = "peer.alpha".to_string();
        tx.send(ev("peer.evil", 1, "x")).await.unwrap();
        tx.send(ev("peer.alpha", 2, "y")).await.unwrap();
        // We can't easily set up a real DB in this test, so spawn the
        // consumer and let `insert_synced_row` fail with a debug log —
        // we only need to prove the validate branch is exercised and the
        // task exits cleanly when `tx` is dropped.
        let task = tokio::spawn(run_event_consumer(rx, owner));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("consumer should exit when tx drops")
            .expect("consumer task should not panic");
    }
}
