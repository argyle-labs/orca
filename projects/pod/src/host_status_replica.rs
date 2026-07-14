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
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use super::subscribe::HostStatusEvent;
use super::subscribe_client::{Forever, dial_subscribe_host_status, subscribe_with_reconnect};

/// Per-peer mpsc buffer. Sized for short stalls in the DB writer (one
/// `spawn_blocking` insert per event); overflow is acceptable because the
/// owner's retention + watermark let us recover via the legacy puller.
const PEER_CHANNEL_CAPACITY: usize = 128;

/// Initial backoff for the reconnect loop. Doubles to 30s max
/// (see `subscribe_client::next_backoff`).
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// How often the fleet replicator reconciles its per-peer subscription set
/// against the paired-peer table. Subscriptions for newly-paired peers
/// start within this window; subscriptions for departed peers are aborted
/// on the next tick.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the long-lived subscription task for a single peer. Returns the
/// producer task's JoinHandle so the fleet replicator can abort it when
/// the peer is unpaired or departs. Aborting the producer drops `tx`,
/// which makes the consumer task exit naturally.
pub fn spawn_for_peer(peer_id: String, host_addr: String) -> JoinHandle<()> {
    let (tx, rx) = mpsc::channel::<HostStatusEvent>(PEER_CHANNEL_CAPACITY);

    let consumer_peer = peer_id.clone();
    tokio::spawn(async move {
        run_event_consumer(rx, consumer_peer).await;
    });

    tokio::spawn(async move {
        let dialer = |host: String, topic: String, tx: mpsc::Sender<HostStatusEvent>| async move {
            dial_subscribe_host_status(&host, &topic, tx).await
        };
        let _stats =
            subscribe_with_reconnect(host_addr, peer_id, tx, dialer, Forever, INITIAL_BACKOFF)
                .await;
    })
}

/// Drain `rx`, validate each event, and write accepted ones into
/// `host_status` with `source = "synced"`. Exits when `tx` is dropped.
async fn run_event_consumer(mut rx: mpsc::Receiver<HostStatusEvent>, owner_peer_id: String) {
    while let Some(ev) = rx.recv().await {
        let Some(payload) = validate_event(&ev, &owner_peer_id) else {
            // Owner-mismatched event — never write a foreign peer_id into our
            // DB. Fail loud (feedback_fail_loud_logging_levels): a mismatch here
            // means either a compromised peer OR an id-normalization gap we need
            // to see, not swallow. warn with both ids so the cause is diagnosable.
            tracing::warn!(
                event_peer_id = %ev.peer_id,
                expected_peer_id = %owner_peer_id,
                "host_status replica dropped owner-mismatched event"
            );
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
    // Match on the bare machine key so a legacy `peer.<id>` on either side
    // still correlates to the same owner (identity is the machine key).
    if crate::machine_key(&event.peer_id) != crate::machine_key(expected_peer_id) {
        return None;
    }
    Some(event.payload.as_str())
}

/// Decide which peer subscriptions to start and which to stop, given the
/// currently-managed set and the desired set. Pure helper — the actual
/// spawn/abort lives in the tick loop.
fn diff_peer_sets(
    current: &HashSet<String>,
    desired: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let to_add: Vec<String> = desired.difference(current).cloned().collect();
    let to_remove: Vec<String> = current.difference(desired).cloned().collect();
    (to_add, to_remove)
}

/// Spawn the fleet-wide replicator: every [`RECONCILE_INTERVAL`], list
/// active paired peers and reconcile the per-peer subscription registry.
/// Idempotent: only the first invocation starts a task.
///
/// Runs alongside the existing pull-based `host_status_writer::spawn_sync_puller`.
/// `INSERT OR IGNORE` semantics in `host_status::insert_status` make the
/// overlap safe — whichever path lands a `(peer_id, snapshot_at)` row first
/// wins; the other no-ops.
pub fn spawn_fleet_replicator() {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let shutdown = utils::shutdown::token();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(20)) => {}
            _ = shutdown.cancelled() => return,
        }
        let registry: Mutex<HashMap<String, JoinHandle<()>>> = Mutex::new(HashMap::new());
        loop {
            if let Err(e) = reconcile_once(&registry).await {
                tracing::debug!("host_status replica reconcile: {e:#}");
            }
            tokio::select! {
                _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
                _ = shutdown.cancelled() => {
                    for (_, h) in registry.lock().await.drain() {
                        h.abort();
                    }
                    return;
                }
            }
        }
    });
}

async fn reconcile_once(registry: &Mutex<HashMap<String, JoinHandle<()>>>) -> Result<()> {
    let own = system::host_identity::machine_id_short().to_string();
    let peers = tokio::task::spawn_blocking(move || -> Result<Vec<(String, String)>> {
        let conn = ::db::open_default()?;
        let rows = ::db::pod::list_peer_summaries(&conn)?;
        Ok(rows
            .into_iter()
            // Key every subscription by the bare machine key: a peer whose
            // pod_peers row still carries a legacy `peer.<id>` CN publishes its
            // host_status under the BARE id, so subscribing/validating on the
            // prefixed form silently never matches. Normalize here so the topic,
            // the owner-mismatch check, and the row owner all agree on the bare
            // key. (Root cause of stale remote runtime in pod.list.)
            .filter(|p| p.status == "active" && crate::machine_key(&p.peer_id) != own)
            .map(|p| (crate::machine_key(&p.peer_id).to_string(), p.addr))
            .collect())
    })
    .await??;

    let desired: HashSet<String> = peers.iter().map(|(p, _)| p.clone()).collect();
    let addr_lookup: HashMap<String, String> = peers.into_iter().collect();
    let current: HashSet<String> = registry.lock().await.keys().cloned().collect();
    let (to_add, to_remove) = diff_peer_sets(&current, &desired);

    let mut reg = registry.lock().await;
    for pid in to_remove {
        if let Some(handle) = reg.remove(&pid) {
            handle.abort();
        }
    }
    for pid in to_add {
        if let Some(addr) = addr_lookup.get(&pid).cloned() {
            let handle = spawn_for_peer(pid.clone(), addr);
            reg.insert(pid, handle);
        }
    }
    Ok(())
}

async fn insert_synced_row(owner: String, snapshot_at: i64, payload: &str) -> Result<()> {
    let payload = payload.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_default()?;
        let now = utils::time::now().unix_seconds();
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
        let e = ev("alpha", 1, "snap");
        assert_eq!(validate_event(&e, "alpha"), Some("snap"));
    }

    #[test]
    fn validate_rejects_foreign_peer_id() {
        let e = ev("evil", 1, "snap");
        assert!(validate_event(&e, "alpha").is_none());
    }

    #[test]
    fn diff_finds_only_new_peers_when_current_is_empty() {
        let current: HashSet<String> = HashSet::new();
        let mut desired = HashSet::new();
        desired.insert("a".to_string());
        desired.insert("b".to_string());
        let (add, remove) = diff_peer_sets(&current, &desired);
        let mut add = add;
        add.sort();
        assert_eq!(add, vec!["a", "b"]);
        assert!(remove.is_empty());
    }

    #[test]
    fn diff_finds_only_departed_when_desired_is_empty() {
        let mut current = HashSet::new();
        current.insert("a".to_string());
        current.insert("b".to_string());
        let desired = HashSet::new();
        let (add, remove) = diff_peer_sets(&current, &desired);
        assert!(add.is_empty());
        let mut remove = remove;
        remove.sort();
        assert_eq!(remove, vec!["a", "b"]);
    }

    #[test]
    fn diff_handles_partial_overlap() {
        let mut current = HashSet::new();
        current.insert("keep".into());
        current.insert("drop".into());
        let mut desired = HashSet::new();
        desired.insert("keep".into());
        desired.insert("add".into());
        let (add, remove) = diff_peer_sets(&current, &desired);
        assert_eq!(add, vec!["add"]);
        assert_eq!(remove, vec!["drop"]);
    }

    #[tokio::test]
    async fn consumer_filters_foreign_events_and_drops_on_tx_close() {
        // Drive the consumer with a mix of matching + foreign events, then
        // drop the sender. Coverage goal: hit both branches of `validate_event`
        // through `run_event_consumer`, then exit cleanly.
        // The one matching event ("alpha") triggers `insert_synced_row`, which
        // does its db work on a `spawn_blocking` thread — that thread inherits
        // neither task- nor thread-local db-path overrides, so it always opens
        // the real db and the insert fails fast on a constraint (logged at
        // debug, non-fatal). When a daemon holds the rollback-journal (non-WAL)
        // write lock, that open/insert can first block up to `busy_timeout`
        // (5s) before returning. Size the timeout above that so the test is
        // deterministic whether or not a daemon is running; with no daemon (CI)
        // the insert returns immediately. Awaited inline so "exits on tx drop"
        // is still what's asserted.
        let (tx, rx) = mpsc::channel::<HostStatusEvent>(4);
        let owner = "alpha".to_string();
        tx.send(ev("evil", 1, "x")).await.unwrap();
        tx.send(ev("alpha", 2, "y")).await.unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), run_event_consumer(rx, owner))
            .await
            .expect("consumer should exit when tx drops");
    }
}
