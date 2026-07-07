//! Replication engine — drives push-on-write + pull-tick + (later) anti-entropy
//! over a `ReplicationTransport` provided at boot. The engine owns the *what*
//! and *when*; the transport owns the *how* (sign, mTLS dial, wire format).
//!
//! db never knows about envelopes, mTLS, or PKI — it just builds bundles from
//! the local registry and hands them to the transport.

// The replication bundle is intentionally heterogeneous JSON: each registered
// entity has its own typed row schema, and the engine treats the per-entity
// rows opaquely (typing happens inside the derive-generated merge fn). Mirrors
// db::replicate::merge_bundle and replicate_wire::ReplicateBundle in pod.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

/// Address book entry for replication. Transport-agnostic — the engine never
/// cares whether `addr` is `host:port`, a peer_id, or something else; that's
/// the transport's contract.
#[derive(Debug, Clone)]
pub struct TransportPeer {
    pub peer_id: String,
    pub hostname: String,
    pub addr: String,
    /// `None` means the engine will skip this peer (no way to authenticate).
    pub pinned_fp: Option<String>,
}

/// Transport contract that the pod crate (or any future transport) implements
/// and registers with the engine via [`register`]. Push and fetch return
/// already-verified entity bundles — the transport hides signing.
#[async_trait]
pub trait ReplicationTransport: Send + Sync + 'static {
    /// All paired non-departed peers (excluding self).
    async fn list_peers(&self) -> Result<Vec<TransportPeer>>;
    /// Push our local bundle to a peer; returns rows merged remotely. The
    /// transport signs internally before sending.
    async fn push(&self, peer: &TransportPeer, bundle: &BTreeMap<String, Value>) -> Result<usize>;
    /// Fetch and verify a peer's bundle. Engine just merges the result.
    async fn fetch(&self, peer: &TransportPeer) -> Result<BTreeMap<String, Value>>;
    /// Fetch a peer's per-entity content roots (cheap divergence check).
    /// Returns `entity_name -> hex sha256 of canonical row serialization`.
    async fn fetch_roots(&self, peer: &TransportPeer) -> Result<BTreeMap<String, String>>;
}

static TRANSPORT: OnceLock<Arc<dyn ReplicationTransport>> = OnceLock::new();

/// Install the transport. Called once at daemon boot, before [`spawn`]. Late
/// callers are ignored (returns Err) — the first registration wins so tests
/// can't accidentally clobber a real transport.
pub fn register(transport: Arc<dyn ReplicationTransport>) -> Result<()> {
    TRANSPORT
        .set(transport)
        .map_err(|_| anyhow::anyhow!("replication transport already registered"))
}

fn transport() -> Option<Arc<dyn ReplicationTransport>> {
    TRANSPORT.get().cloned()
}

/// Per-peer outcome of a single sync (push or pull). `pod sync` returns these
/// directly so operators see exactly what happened.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerSyncReport {
    pub peer_id: String,
    pub hostname: String,
    /// `in_sync` (0 merged), `merged` (n>0), `skipped`, `error`.
    pub status: String,
    pub merged: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub duration_ms: u64,
}

/// Pull tick interval. Push-on-write is the primary path; the pull tick is a
/// backstop for missed pushes (peer was offline, transient errors, etc). At
/// 5s with Merkle-root short-circuiting, in-sync ticks are sub-second and
/// cheap. Roots match → no bundle transfer.
const PULL_INTERVAL: Duration = Duration::from_secs(5);
const INITIAL_DELAY: Duration = Duration::from_secs(3);
const PUSH_COALESCE_WINDOW: Duration = Duration::from_millis(50);

/// Spawn background tasks: push-on-write listener, periodic pull tick, and a
/// one-shot boot push (anti-entropy — peer state propagates as soon as we
/// come back online). No-op if no transport is registered.
pub fn spawn() -> Option<(
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
)> {
    let _t = transport()?;
    let pull = tokio::spawn(pull_loop());
    let push = tokio::spawn(push_loop());
    let boot = tokio::spawn(boot_anti_entropy());
    info!(
        "[replicate] engine armed — push-on-write + {}s pull backstop + boot anti-entropy",
        PULL_INTERVAL.as_secs()
    );
    Some((pull, push, boot))
}

/// Send our current state to every paired peer once at boot. Covers the
/// "this host was offline and now it's back" case — peers learn our latest
/// rows without waiting for the next origin write.
async fn boot_anti_entropy() {
    let shutdown = utils::shutdown::token();
    tokio::select! {
        _ = tokio::time::sleep(INITIAL_DELAY) => {}
        _ = shutdown.cancelled() => return,
    }
    if let Err(e) = push_now().await {
        warn!("[replicate.boot] anti-entropy push failed: {e:#}");
    } else {
        debug!("[replicate.boot] anti-entropy push complete");
    }
}

async fn pull_loop() {
    let shutdown = utils::shutdown::token();
    tokio::select! {
        _ = tokio::time::sleep(INITIAL_DELAY) => {}
        _ = shutdown.cancelled() => return,
    }
    let mut ticker = tokio::time::interval(PULL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.cancelled() => return,
        }
        match sync_now(None).await {
            Ok(reports) => {
                for r in reports {
                    match r.status.as_str() {
                        "merged" => info!(
                            "[replicate.pull] merged {} row(s) from {}",
                            r.merged, r.hostname
                        ),
                        "error" => warn!(
                            "[replicate.pull] fetch from {} failed: {}",
                            r.hostname,
                            r.error.as_deref().unwrap_or("")
                        ),
                        _ => {}
                    }
                }
            }
            Err(e) => warn!("[replicate.pull] tick aborted: {e:#}"),
        }
    }
}

async fn push_loop() {
    let mut rx = crate::replicate::subscribe();
    let shutdown = utils::shutdown::token();
    loop {
        let recv = tokio::select! {
            r = rx.recv() => r,
            _ = shutdown.cancelled() => return,
        };
        match recv {
            Ok(_entity) => {
                tokio::select! {
                    _ = tokio::time::sleep(PUSH_COALESCE_WINDOW) => {}
                    _ = shutdown.cancelled() => return,
                }
                while rx.try_recv().is_ok() {}
                if let Err(e) = push_now().await {
                    warn!("[replicate.push] push failed: {e:#}");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("[replicate.push] lagged {n} notifications — forcing one push");
                if let Err(e) = push_now().await {
                    warn!("[replicate.push] catch-up push failed: {e:#}");
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Run one pull tick now. Optional peer filter (hostname / peer_id / addr).
/// Used by `pod sync` and by the background pull loop.
pub async fn sync_now(peer_filter: Option<&str>) -> Result<Vec<PeerSyncReport>> {
    let Some(t) = transport() else {
        anyhow::bail!("no replication transport registered — pair this host first");
    };
    let peers = t.list_peers().await?;
    let mut reports = Vec::with_capacity(peers.len());
    for p in peers {
        if let Some(f) = peer_filter
            && p.hostname != f
            && p.peer_id != f
            && p.addr != f
        {
            continue;
        }
        let started = Instant::now();
        let elapsed = || started.elapsed().as_millis() as u64;
        if p.pinned_fp.is_none() {
            reports.push(PeerSyncReport {
                peer_id: p.peer_id,
                hostname: p.hostname,
                status: "skipped".into(),
                merged: 0,
                error: None,
                skip_reason: Some("no pinned bootstrap pubkey_fp".into()),
                duration_ms: elapsed(),
            });
            continue;
        }
        let local_roots = {
            let conn = crate::open_default()?;
            crate::replicate::roots(&conn)?
        };
        let remote_roots = match t.fetch_roots(&p).await {
            Ok(r) => r,
            Err(e) => {
                reports.push(PeerSyncReport {
                    peer_id: p.peer_id,
                    hostname: p.hostname,
                    status: "error".into(),
                    merged: 0,
                    error: Some(format!("fetch_roots: {e:#}")),
                    skip_reason: None,
                    duration_ms: elapsed(),
                });
                continue;
            }
        };
        if local_roots == remote_roots {
            reports.push(PeerSyncReport {
                peer_id: p.peer_id,
                hostname: p.hostname,
                status: "in_sync".into(),
                merged: 0,
                error: None,
                skip_reason: None,
                duration_ms: elapsed(),
            });
            continue;
        }
        let report = match t.fetch(&p).await {
            Ok(bundle) => {
                let conn = crate::open_default()?;
                match crate::replicate::merge_bundle(&conn, bundle) {
                    Ok(0) => PeerSyncReport {
                        peer_id: p.peer_id.clone(),
                        hostname: p.hostname.clone(),
                        status: "in_sync".into(),
                        merged: 0,
                        error: None,
                        skip_reason: None,
                        duration_ms: elapsed(),
                    },
                    Ok(n) => PeerSyncReport {
                        peer_id: p.peer_id.clone(),
                        hostname: p.hostname.clone(),
                        status: "merged".into(),
                        merged: n,
                        error: None,
                        skip_reason: None,
                        duration_ms: elapsed(),
                    },
                    Err(e) => PeerSyncReport {
                        peer_id: p.peer_id.clone(),
                        hostname: p.hostname.clone(),
                        status: "error".into(),
                        merged: 0,
                        error: Some(format!("merge: {e:#}")),
                        skip_reason: None,
                        duration_ms: elapsed(),
                    },
                }
            }
            Err(e) => PeerSyncReport {
                peer_id: p.peer_id.clone(),
                hostname: p.hostname.clone(),
                status: "error".into(),
                merged: 0,
                error: Some(format!("fetch: {e:#}")),
                skip_reason: None,
                duration_ms: elapsed(),
            },
        };
        reports.push(report);
        // Divergence detected (remote≠local roots): we just pulled from them
        // to repair our side; also push to them so theirs converges too. This
        // is the row-level "loser overwrites" anti-entropy from the design.
        // LWW inside merge_bundle decides the actual winner on each side.
        let bundle = {
            let conn = crate::open_default()?;
            crate::replicate::export_all(&conn)?
        };
        if let Err(e) = t.push(&p, &bundle).await {
            debug!(
                "[replicate.repair] mutual-push to {} failed: {e:#}",
                p.hostname
            );
        }
    }
    Ok(reports)
}

/// Push the current local bundle to every paired peer in parallel.
pub async fn push_now() -> Result<()> {
    let Some(t) = transport() else { return Ok(()) };
    let conn = crate::open_default()?;
    let bundle = crate::replicate::export_all(&conn)?;
    drop(conn);
    let peers = t.list_peers().await?;
    let mut handles = Vec::with_capacity(peers.len());
    for p in peers {
        if p.pinned_fp.is_none() {
            continue;
        }
        let t = Arc::clone(&t);
        let bundle = bundle.clone();
        handles.push(tokio::spawn(async move {
            match t.push(&p, &bundle).await {
                Ok(n) => debug!("[replicate.push] {} accepted {n} row(s)", p.hostname),
                Err(e) => warn!("[replicate.push] push to {} failed: {e:#}", p.hostname),
            }
        }));
    }
    for h in handles {
        drop(h.await);
    }
    Ok(())
}

/// Helper for receiver-side handlers: given an already-verified bundle, merge
/// it into the local DB and return rows merged. The signature/fp check stays
/// in the transport — engine just owns the persistence step.
pub fn merge_into_local(bundle: BTreeMap<String, Value>) -> Result<usize> {
    let conn = crate::open_default()?;
    crate::replicate::merge_bundle(&conn, bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-memory transport stand-in. Mutable inner state so each test can
    /// reconfigure between calls without re-installing the global slot.
    #[derive(Default)]
    struct FakeInner {
        peers: Vec<TransportPeer>,
        remote_roots: BTreeMap<String, String>,
        remote_bundle: BTreeMap<String, Value>,
        fetch_roots_err: bool,
        fetch_err: bool,
        push_log: Vec<String>,
        fetch_roots_calls: usize,
        fetch_calls: usize,
    }

    #[derive(Default)]
    struct FakeTransport(Mutex<FakeInner>);

    impl FakeTransport {
        fn with(inner: FakeInner) -> Arc<Self> {
            Arc::new(Self(Mutex::new(inner)))
        }
        fn set(&self, inner: FakeInner) {
            *self.0.lock().unwrap() = inner;
        }
        fn snapshot<R>(&self, f: impl FnOnce(&FakeInner) -> R) -> R {
            f(&self.0.lock().unwrap())
        }
    }

    #[async_trait]
    impl ReplicationTransport for FakeTransport {
        async fn list_peers(&self) -> Result<Vec<TransportPeer>> {
            Ok(self.0.lock().unwrap().peers.clone())
        }
        async fn push(
            &self,
            peer: &TransportPeer,
            _bundle: &BTreeMap<String, Value>,
        ) -> Result<usize> {
            self.0.lock().unwrap().push_log.push(peer.hostname.clone());
            Ok(0)
        }
        async fn fetch(&self, _peer: &TransportPeer) -> Result<BTreeMap<String, Value>> {
            let mut g = self.0.lock().unwrap();
            g.fetch_calls += 1;
            if g.fetch_err {
                anyhow::bail!("fetch failed");
            }
            Ok(g.remote_bundle.clone())
        }
        async fn fetch_roots(&self, _peer: &TransportPeer) -> Result<BTreeMap<String, String>> {
            let mut g = self.0.lock().unwrap();
            g.fetch_roots_calls += 1;
            if g.fetch_roots_err {
                anyhow::bail!("fetch_roots failed");
            }
            Ok(g.remote_roots.clone())
        }
    }

    fn peer(host: &str, pinned: Option<&str>) -> TransportPeer {
        TransportPeer {
            peer_id: host.into(),
            hostname: host.into(),
            addr: format!("10.0.0.{host}"),
            pinned_fp: pinned.map(String::from),
        }
    }

    // Engine reads transport via a process-global OnceLock. Tests install a
    // forwarding shim once; each test swaps the active FakeTransport into a
    // Mutex slot that the shim reads from. The shim is registered on first
    // use and stays for the process lifetime — clean enough for unit tests.
    fn current_slot() -> &'static Mutex<Option<Arc<FakeTransport>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<FakeTransport>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    struct Shim;

    #[async_trait]
    impl ReplicationTransport for Shim {
        async fn list_peers(&self) -> Result<Vec<TransportPeer>> {
            let f = current_slot()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .expect("engine test ran without with_engine slot installed")
                .clone();
            f.list_peers().await
        }
        async fn push(&self, p: &TransportPeer, b: &BTreeMap<String, Value>) -> Result<usize> {
            let f = current_slot()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .expect("engine test ran without with_engine slot installed")
                .clone();
            f.push(p, b).await
        }
        async fn fetch(&self, p: &TransportPeer) -> Result<BTreeMap<String, Value>> {
            let f = current_slot()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .expect("engine test ran without with_engine slot installed")
                .clone();
            f.fetch(p).await
        }
        async fn fetch_roots(&self, p: &TransportPeer) -> Result<BTreeMap<String, String>> {
            let f = current_slot()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
                .expect("engine test ran without with_engine slot installed")
                .clone();
            f.fetch_roots(p).await
        }
    }

    /// Register the test Shim engine exactly once. `register` errors when an
    /// engine is already installed; that's the expected path on the 2nd+
    /// engine test in this module. Guarding with `Once` ensures the first
    /// register panics on real failures and later calls are no-ops.
    fn install_shim_once() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            register(Arc::new(Shim)).expect("register test shim engine");
        });
    }

    /// Process-wide serializer for engine tests. The `Shim` reads transport
    /// state through a single global slot, so two tests running in parallel
    /// would trample each other (one sets `Some(fake_a)` mid-await while the
    /// other expects `Some(fake_b)`). Each engine test acquires this for the
    /// duration of its body. `tokio::sync::Mutex` lets us hold it across
    /// `.await` points; poison recovery isn't needed since panics simply
    /// drop the guard.
    fn engine_test_lock() -> &'static tokio::sync::Mutex<()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    async fn with_engine<F, Fut>(fake: Arc<FakeTransport>, body: F)
    where
        F: FnOnce(Arc<FakeTransport>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _guard = engine_test_lock().lock().await;
        install_shim_once();
        // Recover from any prior-test panic that left the slot Mutex poisoned.
        let slot = current_slot();
        {
            let mut g = slot.lock().unwrap_or_else(|p| p.into_inner());
            *g = Some(Arc::clone(&fake));
        }
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("orca.db");
        crate::with_db_path(db_path, body(fake)).await;
        let mut g = slot.lock().unwrap_or_else(|p| p.into_inner());
        *g = None;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_skips_peer_with_no_pinned_fp() {
        let fake = FakeTransport::with(FakeInner {
            peers: vec![peer("alpha", None)],
            ..Default::default()
        });
        with_engine(fake, |f| async move {
            let reports = sync_now(None).await.unwrap();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].status, "skipped");
            assert!(reports[0].skip_reason.is_some());
            f.snapshot(|i| {
                assert_eq!(i.fetch_roots_calls, 0);
                assert_eq!(i.fetch_calls, 0);
            });
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_in_sync_when_roots_match_skips_fetch() {
        let fake = FakeTransport::with(FakeInner {
            peers: vec![peer("alpha", Some("fp"))],
            ..Default::default()
        });
        with_engine(fake, |f| async move {
            let conn = crate::open_default().unwrap();
            crate::users::insert(&conn, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z")
                .unwrap();
            let local = crate::replicate::roots(&conn).unwrap();
            drop(conn);
            f.set(FakeInner {
                peers: vec![peer("alpha", Some("fp"))],
                remote_roots: local,
                ..Default::default()
            });
            let r = sync_now(None).await.unwrap();
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].status, "in_sync");
            assert_eq!(r[0].merged, 0);
            f.snapshot(|i| {
                assert_eq!(i.fetch_roots_calls, 1);
                assert_eq!(i.fetch_calls, 0, "matching roots must skip fetch");
            });
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_merges_and_mutual_pushes_on_divergence() {
        // Remote has a row our local doesn't — roots differ, we fetch, merge,
        // then push back.
        let donor = crate::testing::test_conn();
        crate::users::insert(&donor, "u1", "scott", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        let remote_bundle = crate::replicate::export_all(&donor).unwrap();
        let remote_roots = crate::replicate::roots(&donor).unwrap();

        let fake = FakeTransport::with(FakeInner {
            peers: vec![peer("alpha", Some("fp"))],
            remote_roots,
            remote_bundle,
            ..Default::default()
        });
        with_engine(fake, |f| async move {
            let r = sync_now(None).await.unwrap();
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].status, "merged");
            assert_eq!(r[0].merged, 1);
            f.snapshot(|i| {
                assert_eq!(i.fetch_roots_calls, 1);
                assert_eq!(i.fetch_calls, 1);
                assert_eq!(i.push_log, vec!["alpha".to_string()]);
            });
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_filters_by_peer() {
        let fake = FakeTransport::with(FakeInner {
            peers: vec![
                peer("alpha", Some("fp")),
                peer("beta", Some("fp")),
                peer("gamma", Some("fp")),
            ],
            ..Default::default()
        });
        with_engine(fake, |_f| async move {
            let r = sync_now(Some("beta")).await.unwrap();
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].hostname, "beta");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_returns_error_when_fetch_roots_fails() {
        let fake = FakeTransport::with(FakeInner {
            peers: vec![peer("alpha", Some("fp"))],
            fetch_roots_err: true,
            ..Default::default()
        });
        with_engine(fake, |_f| async move {
            let r = sync_now(None).await.unwrap();
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].status, "error");
            assert!(r[0].error.as_ref().unwrap().contains("fetch_roots"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sync_now_returns_error_when_bundle_fetch_fails() {
        let donor = crate::testing::test_conn();
        crate::users::insert(&donor, "u1", "x", "h", "admin", "2026-01-01T00:00:00Z").unwrap();
        let diverging_roots = crate::replicate::roots(&donor).unwrap();

        let fake = FakeTransport::with(FakeInner {
            peers: vec![peer("alpha", Some("fp"))],
            remote_roots: diverging_roots,
            fetch_err: true,
            ..Default::default()
        });
        with_engine(fake, |_f| async move {
            let r = sync_now(None).await.unwrap();
            assert_eq!(r[0].status, "error");
            assert!(r[0].error.as_ref().unwrap().contains("fetch"));
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn push_now_fans_out_to_peers_with_pinned_fp_only() {
        let fake = FakeTransport::with(FakeInner {
            peers: vec![
                peer("alpha", Some("fp")),
                peer("beta", None), // no fp -> skipped
                peer("gamma", Some("fp")),
            ],
            ..Default::default()
        });
        with_engine(fake, |f| async move {
            push_now().await.unwrap();
            let mut log = f.snapshot(|i| i.push_log.clone());
            log.sort();
            assert_eq!(log, vec!["alpha".to_string(), "gamma".to_string()]);
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn merge_into_local_persists_a_bundle() {
        let donor = crate::testing::test_conn();
        crate::users::insert(&donor, "u1", "bob", "h", "member", "2026-01-01T00:00:00Z").unwrap();
        let bundle = crate::replicate::export_all(&donor).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("orca.db");
        crate::with_db_path(db_path.clone(), async {
            let n = merge_into_local(bundle).unwrap();
            assert_eq!(n, 1);
            let conn = crate::open_default().unwrap();
            let u = crate::users::find_auth_by_username(&conn, "bob")
                .unwrap()
                .unwrap();
            assert_eq!(u.id, "u1");
        })
        .await;
    }

    #[test]
    fn peer_sync_report_omits_none_fields_in_json() {
        let r = PeerSyncReport {
            peer_id: "p".into(),
            hostname: "h".into(),
            status: "in_sync".into(),
            merged: 0,
            error: None,
            skip_reason: None,
            duration_ms: 1,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("\"error\""), "error: null leaked: {s}");
        assert!(!s.contains("\"skip_reason\""), "skip_reason: null leaked");
    }
}
