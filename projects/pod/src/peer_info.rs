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
pub async fn peer_detail(peer_id: &str, addr: &str, force: bool) -> Result<SystemStatusReport> {
    get_or_fetch(&DETAIL_CACHE, peer_id, DETAIL_TTL, force, || async {
        let res = crate::exec(addr, "system.detail", serde_json::json!({})).await?;
        let report: SystemStatusReport =
            serde_json::from_value(res.result).context("decode system.detail response")?;
        Ok(report)
    })
    .await
}

/// Fetch (or serve from cache) a peer's `system.update` state.
pub async fn peer_update(peer_id: &str, addr: &str, force: bool) -> Result<PeerUpdateFields> {
    get_or_fetch(&UPDATE_CACHE, peer_id, UPDATE_TTL, force, || async {
        let res = crate::exec(addr, "system.update", serde_json::json!({})).await?;
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

/// Resolve a peer_id to its dial addr via the local `pod_peers` row, then
/// force-refresh its `system.detail`. Used after `system.update --peer <h>`
/// completes so the next `pod.list` reflects the new version immediately rather
/// than waiting out the detail TTL.
pub async fn refresh_peer(peer_id: &str) -> Result<()> {
    let pid = peer_id.to_string();
    let addr = tokio::task::spawn_blocking(move || -> Result<String> {
        let conn = db::open_default()?;
        let peers = db::pod::list_peer_summaries(&conn)?;
        peers
            .into_iter()
            .find(|p| p.peer_id == pid)
            .map(|p| p.addr)
            .ok_or_else(|| anyhow::anyhow!("peer {pid} not in pod_peers"))
    })
    .await??;
    peer_detail(peer_id, &addr, true).await?;
    Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
