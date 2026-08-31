//! On-demand, per-peer observed-state cache.
//!
//! Replaces the three eager peer-telemetry loops (host_status sync puller +
//! fleet subscription replica, `system.detail` probe, `system.update` probe)
//! that used to poll every peer on a timer and mirror the results into DB
//! tables. Under the data-classification law, observed/telemetry state is NOT
//! synced across the mesh: each node answers live queries about *itself*, and a
//! consumer (e.g. `pod.list`) fetches what it needs ON DEMAND, caching the
//! result in memory with a short per-datum TTL and a force-refresh escape.
//!
//! Nothing here is persisted — the cache is rebuilt lazily on the next read
//! after a restart. Cardinality is bounded by live peers via [`remove`] /
//! [`retain_only`], wired into the peer-retirement and reconcile paths.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use system::system::SystemStatusReport;

/// TTL for a peer's `system.detail` snapshot (runtime fields + OS SystemInfo).
/// Medium: `system.detail` is the heavier call (storage walk, diagnostics), and
/// the fields it carries — version/mode/channel and the OS snapshot — change
/// only on an update or gradual resource drift. A force-refresh (post-update)
/// bypasses this, so 120s is fine for the passive dashboard path.
pub const DETAIL_TTL: Duration = Duration::from_secs(120);

/// TTL for a peer's `system.update` result. Long: update availability only
/// changes when an operator applies an update or a new release lands upstream,
/// and the fetch fans out to GitHub. 10 minutes keeps it cheap.
pub const UPDATE_TTL: Duration = Duration::from_secs(600);

/// TTL floor for the most volatile run-state view. Reserved for callers that
/// want a near-live status read; the detail fetch accepts an explicit TTL so a
/// caller can request this shorter window when freshness matters.
pub const HOST_STATUS_TTL: Duration = Duration::from_secs(15);

/// TTL for a peer's liveness datum (reachability + version). Short — it is the
/// most volatile field and it backs the thin `pod.list`/`systems.list` roster.
/// A background refresher repopulates it on an interval; the READ path serves
/// whatever is cached and NEVER dials, so the roster read stays within the
/// latency budget. Slightly longer than the refresh interval so a single missed
/// tick doesn't blank the roster.
pub const PING_TTL: Duration = Duration::from_secs(30);

/// One cached value with the monotonic instant it was fetched.
#[derive(Clone)]
struct CacheEntry<T> {
    value: T,
    fetched_at: Instant,
}

/// Pure freshness decision — extracted so the TTL logic is unit-testable
/// without a network round-trip or a mockable clock.
fn is_fresh(fetched_at: Instant, now: Instant, ttl: Duration) -> bool {
    now.duration_since(fetched_at) < ttl
}

static DETAIL_CACHE: LazyLock<RwLock<HashMap<String, CacheEntry<SystemStatusReport>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

static UPDATE_CACHE: LazyLock<RwLock<HashMap<String, CacheEntry<PeerUpdateFields>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// The liveness slice a roster row needs: reachability + version + last probe
/// error. Populated by the background refresher, read (never fetched) on the
/// `pod.list`/`systems.list` read path.
#[derive(Clone, Default)]
pub struct PeerLiveness {
    pub reachable: bool,
    pub version: Option<String>,
    pub probe_error: Option<String>,
}

static PING_CACHE: LazyLock<RwLock<HashMap<String, CacheEntry<PeerLiveness>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Read a peer's cached liveness WITHOUT dialing. Returns `None` when there is
/// no fresh probe result (younger than [`PING_TTL`]) — the caller then renders
/// reachability as unknown rather than paying a network round-trip on the read
/// path. This is the invariant that keeps the roster read within budget:
/// probing happens only in the background refresher, never inline.
pub fn liveness_if_fresh(peer_id: &str) -> Option<PeerLiveness> {
    if let Ok(g) = PING_CACHE.read()
        && let Some(entry) = g.get(peer_id)
        && is_fresh(entry.fetched_at, Instant::now(), PING_TTL)
    {
        return Some(entry.value.clone());
    }
    None
}

/// Store a fresh liveness probe result. Called by the background refresher after
/// each per-peer probe.
pub fn put_liveness(peer_id: &str, value: PeerLiveness) {
    if let Ok(mut g) = PING_CACHE.write() {
        g.insert(
            peer_id.to_string(),
            CacheEntry {
                value,
                fetched_at: Instant::now(),
            },
        );
    }
}

/// Re-stamp a peer's existing cached liveness as fresh WITHOUT probing. Used when
/// the refresher skips a dial because the peer is in probe-backoff: the roster
/// keeps showing the last-known state (typically `reachable:false`) instead of
/// decaying to unknown, and we still don't pay a network round-trip. No-op if we
/// have never had a result for the peer.
pub fn touch_liveness(peer_id: &str) {
    if let Ok(mut g) = PING_CACHE.write()
        && let Some(entry) = g.get_mut(peer_id)
    {
        entry.fetched_at = Instant::now();
    }
}

/// Base probe interval after a peer first goes unreachable. The refresher's own
/// cadence (10s) is the floor for a *reachable* peer; once a peer starts failing
/// we widen from here so a dead peer isn't dialed every pass.
const PROBE_BACKOFF_BASE: Duration = Duration::from_secs(30);
/// Ceiling on the probe interval for a persistently-unreachable peer — one dial
/// every 5 min is enough to notice it come back while eliminating the churn.
const PROBE_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Per-peer probe schedule: when we're next allowed to dial, and how many
/// consecutive failures have accrued (drives the exponential widening).
struct ProbeSchedule {
    next_probe_at: Instant,
    fail_streak: u32,
}

static PROBE_SCHED: LazyLock<RwLock<HashMap<String, ProbeSchedule>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Whether the refresher should dial this peer on the current pass. A peer with
/// no schedule entry (never failed, or recently succeeded) is always probed;
/// one in backoff is skipped until its window elapses.
pub fn should_probe(peer_id: &str, now: Instant) -> bool {
    match PROBE_SCHED.read() {
        Ok(g) => g.get(peer_id).is_none_or(|s| now >= s.next_probe_at),
        Err(_) => true,
    }
}

/// Record a probe outcome and update the schedule. Success clears the backoff so
/// the peer returns to every-pass probing (and its down→up recovery is noticed
/// promptly). Failure widens the next-probe window exponentially, capped at
/// [`PROBE_BACKOFF_MAX`].
pub fn record_probe_result(peer_id: &str, ok: bool, now: Instant) {
    if let Ok(mut g) = PROBE_SCHED.write() {
        if ok {
            g.remove(peer_id);
            return;
        }
        let entry = g.entry(peer_id.to_string()).or_insert(ProbeSchedule {
            next_probe_at: now,
            fail_streak: 0,
        });
        entry.fail_streak = entry.fail_streak.saturating_add(1);
        let delay = PROBE_BACKOFF_BASE
            .saturating_mul(1u32 << (entry.fail_streak - 1).min(4))
            .min(PROBE_BACKOFF_MAX);
        entry.next_probe_at = now + delay;
    }
}

/// The update-state slice a `pod.list` row needs, distilled from
/// `SystemUpdateOutput`. Mirrors what the retired `peer_update_state` table
/// stored, minus the DB plumbing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerUpdateFields {
    pub version: Option<String>,
    pub channel: Option<String>,
    pub pinned_to: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    /// Wall-clock unix seconds of the successful fetch (for the UI "checked N
    /// seconds ago" age).
    pub checked_at: i64,
}

/// Fetch (or serve from cache) a peer's `system.detail`. `force` bypasses the
/// TTL — used after an update applies so the new version surfaces at once.
pub async fn peer_detail(peer_id: &str, force: bool) -> Result<SystemStatusReport> {
    get_or_fetch(&DETAIL_CACHE, peer_id, DETAIL_TTL, force, || async {
        let res = crate::exec_peer(peer_id, "system.detail", serde_json::json!({})).await?;
        let report: SystemStatusReport =
            serde_json::from_value(res.result).context("decode system.detail response")?;
        Ok(report)
    })
    .await
}

/// Fetch (or serve from cache) a peer's `system.update` state.
pub async fn peer_update(peer_id: &str, force: bool) -> Result<PeerUpdateFields> {
    get_or_fetch(&UPDATE_CACHE, peer_id, UPDATE_TTL, force, || async {
        let res = crate::exec_peer(peer_id, "system.update", serde_json::json!({})).await?;
        let out: system::commands::SystemUpdateOutput =
            serde_json::from_value(res.result).context("decode SystemUpdateOutput")?;
        Ok(update_fields_from_output(out))
    })
    .await
}

/// Distill `SystemUpdateOutput` into the cached slice. `current_version` is
/// canonical (no `v` prefix); `latest` is kept verbatim from the release tag.
/// Prefer the peer's own server-side `update_available` flag so list and detail
/// views never disagree; fall back to recomputing for older peers.
fn update_fields_from_output(out: system::commands::SystemUpdateOutput) -> PeerUpdateFields {
    let version = (!out.current_version.is_empty()).then_some(out.current_version.clone());
    let channel = (!out.channel.is_empty()).then_some(out.channel.clone());
    let latest = out.latest.clone();
    let update_available = out
        .update_available
        .unwrap_or_else(|| match (&version, &latest) {
            (Some(v), Some(l)) => system::update_state::is_update_available(v, l),
            _ => false,
        });
    PeerUpdateFields {
        version,
        channel,
        pinned_to: out.pinned_to,
        latest,
        update_available,
        checked_at: utils::time::now().unix_seconds(),
    }
}

/// Generic get-or-fetch over a per-peer cache. Serves a cached value while it's
/// younger than `ttl` (unless `force`), otherwise runs `fetch`, stores the
/// result, and returns it. A failed fetch does NOT evict the prior entry — the
/// error propagates and the last good value survives for the next attempt.
async fn get_or_fetch<T, F, Fut>(
    cache: &RwLock<HashMap<String, CacheEntry<T>>>,
    peer_id: &str,
    ttl: Duration,
    force: bool,
    fetch: F,
) -> Result<T>
where
    T: Clone,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if !force
        && let Ok(g) = cache.read()
        && let Some(entry) = g.get(peer_id)
        && is_fresh(entry.fetched_at, Instant::now(), ttl)
    {
        return Ok(entry.value.clone());
    }
    let value = fetch().await?;
    if let Ok(mut g) = cache.write() {
        g.insert(
            peer_id.to_string(),
            CacheEntry {
                value: value.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    Ok(value)
}

/// Drop a peer's cached observed-state (all datums). Called from peer-retirement
/// paths (pod kick / leave / forget) so cardinality stays bounded by live peers.
pub fn remove(peer_id: &str) {
    if let Ok(mut g) = DETAIL_CACHE.write() {
        g.remove(peer_id);
    }
    if let Ok(mut g) = UPDATE_CACHE.write() {
        g.remove(peer_id);
    }
    if let Ok(mut g) = PING_CACHE.write() {
        g.remove(peer_id);
    }
}

/// Retain only the supplied peer ids; evict everything else. Called from the
/// periodic peer reconcile to GC entries whose peer row has been removed.
pub fn retain_only(active_peer_ids: &std::collections::HashSet<String>) {
    if let Ok(mut g) = DETAIL_CACHE.write() {
        g.retain(|k, _| active_peer_ids.contains(k));
    }
    if let Ok(mut g) = UPDATE_CACHE.write() {
        g.retain(|k, _| active_peer_ids.contains(k));
    }
    if let Ok(mut g) = PING_CACHE.write() {
        g.retain(|k, _| active_peer_ids.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn probe_schedule_backs_off_on_failure_and_clears_on_success() {
        let peer = "probe-sched-peer";
        let t0 = Instant::now();
        // No history → always probe.
        assert!(should_probe(peer, t0));
        // A failure arms the backoff: not due again until the base window elapses.
        record_probe_result(peer, false, t0);
        assert!(!should_probe(peer, t0));
        assert!(!should_probe(peer, t0 + Duration::from_secs(29)));
        assert!(should_probe(peer, t0 + PROBE_BACKOFF_BASE));
        // Consecutive failures widen the window (exponential).
        let t1 = t0 + PROBE_BACKOFF_BASE;
        record_probe_result(peer, false, t1);
        assert!(!should_probe(peer, t1 + PROBE_BACKOFF_BASE));
        // A success clears the schedule → back to every-pass probing.
        record_probe_result(peer, true, t1 + PROBE_BACKOFF_MAX);
        assert!(should_probe(peer, t1 + PROBE_BACKOFF_MAX));
    }

    #[test]
    fn probe_backoff_is_capped() {
        let peer = "probe-sched-cap";
        let mut t = Instant::now();
        for _ in 0..12 {
            record_probe_result(peer, false, t);
            t += PROBE_BACKOFF_MAX;
        }
        // Even after many failures the next-probe window never exceeds the cap.
        record_probe_result(peer, false, t);
        assert!(should_probe(peer, t + PROBE_BACKOFF_MAX));
    }

    #[test]
    fn touch_liveness_refreshes_without_a_value_change() {
        let peer = "touch-liveness-peer";
        // No entry → touch is a no-op, still unknown.
        touch_liveness(peer);
        assert!(liveness_if_fresh(peer).is_none());
        put_liveness(
            peer,
            PeerLiveness {
                reachable: false,
                version: None,
                probe_error: Some("down".into()),
            },
        );
        touch_liveness(peer);
        let live = liveness_if_fresh(peer).expect("carried forward");
        assert!(!live.reachable);
        assert_eq!(live.probe_error.as_deref(), Some("down"));
    }

    #[test]
    fn liveness_cache_stores_and_serves_without_dialing() {
        let peer = "liveness-test-peer-a";
        // Nothing cached yet → None (caller renders reachability unknown, no dial).
        assert!(liveness_if_fresh(peer).is_none());
        put_liveness(
            peer,
            PeerLiveness {
                reachable: true,
                version: Some("0.1.7".to_string()),
                probe_error: None,
            },
        );
        let got = liveness_if_fresh(peer).expect("fresh entry served from cache");
        assert!(got.reachable);
        assert_eq!(got.version.as_deref(), Some("0.1.7"));
        // remove() must evict the liveness datum too.
        remove(peer);
        assert!(liveness_if_fresh(peer).is_none());
    }

    #[test]
    fn retain_only_evicts_stale_liveness_entries() {
        let keep = "liveness-test-keep";
        let drop_it = "liveness-test-drop";
        put_liveness(keep, PeerLiveness::default());
        put_liveness(drop_it, PeerLiveness::default());
        let active: std::collections::HashSet<String> = [keep.to_string()].into_iter().collect();
        retain_only(&active);
        assert!(liveness_if_fresh(keep).is_some());
        assert!(liveness_if_fresh(drop_it).is_none());
    }

    #[test]
    fn is_fresh_respects_ttl_boundary() {
        let base = Instant::now();
        let ttl = Duration::from_secs(30);
        // Just under TTL → fresh; at/over TTL → stale.
        assert!(is_fresh(base, base + Duration::from_secs(29), ttl));
        assert!(!is_fresh(base, base + Duration::from_secs(30), ttl));
        assert!(!is_fresh(base, base + Duration::from_secs(31), ttl));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fresh_entry_served_from_cache_without_fetch() {
        let cache: RwLock<HashMap<String, CacheEntry<u64>>> = RwLock::new(HashMap::new());
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(7u64)
        };
        // First call populates.
        let v1 = get_or_fetch(&cache, "p", Duration::from_secs(300), false, fetch)
            .await
            .unwrap();
        assert_eq!(v1, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Second call within TTL serves cache — fetch NOT called again.
        let fetch2 = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(99u64)
        };
        let v2 = get_or_fetch(&cache, "p", Duration::from_secs(300), false, fetch2)
            .await
            .unwrap();
        assert_eq!(v2, 7, "served the cached value, not the new fetch");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no second fetch");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_bypasses_a_fresh_entry() {
        let cache: RwLock<HashMap<String, CacheEntry<u64>>> = RwLock::new(HashMap::new());
        get_or_fetch(&cache, "p", Duration::from_secs(300), false, || async {
            Ok(1u64)
        })
        .await
        .unwrap();
        let v = get_or_fetch(&cache, "p", Duration::from_secs(300), true, || async {
            Ok(2u64)
        })
        .await
        .unwrap();
        assert_eq!(v, 2, "force refetched despite a fresh entry");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_entry_triggers_refetch() {
        let cache: RwLock<HashMap<String, CacheEntry<u64>>> = RwLock::new(HashMap::new());
        // Seed a manually-aged entry (fetched well in the past) with a tiny TTL.
        {
            let mut g = cache.write().unwrap();
            g.insert(
                "p".into(),
                CacheEntry {
                    value: 1u64,
                    fetched_at: Instant::now() - Duration::from_secs(10),
                },
            );
        }
        let v = get_or_fetch(&cache, "p", Duration::from_millis(1), false, || async {
            Ok(2u64)
        })
        .await
        .unwrap();
        assert_eq!(v, 2, "stale entry was refetched");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_fetch_preserves_prior_value() {
        let cache: RwLock<HashMap<String, CacheEntry<u64>>> = RwLock::new(HashMap::new());
        // Seed a deterministically-stale entry (fetched in the past).
        cache.write().unwrap().insert(
            "p".into(),
            CacheEntry {
                value: 5u64,
                fetched_at: Instant::now() - Duration::from_secs(10),
            },
        );
        // Stale + failing fetch → error, but the cached entry stays untouched.
        let err = get_or_fetch(&cache, "p", Duration::from_millis(1), false, || async {
            anyhow::bail!("boom")
        })
        .await;
        assert!(err.is_err());
        assert_eq!(cache.read().unwrap().get("p").unwrap().value, 5);
    }
}
