//! In-memory, per-vantage route-health cache for peer dialing.
//!
//! Address reachability is **directional**: a peer's `fd00::/…` ULA IPv6 may be
//! reachable from a host on the same L2 but permanently dead from another
//! segment. So this cache is keyed `(peer_id, address)` **from THIS host's
//! vantage** and is deliberately **local + in-memory only** — never persisted,
//! never replicated onto the mesh (that would be a global up/down flag, which is
//! wrong). It rebuilds on restart and self-warms on the first probe tick.
//!
//! Two consumers:
//!   * [`reorder`] — health-aware sort applied on top of the pure preference
//!     order in `dialer::select_dial_targets`, so a recently-good address is
//!     tried first and a repeatedly-failing one sinks to last (never dropped —
//!     reachability can return).
//!   * [`record_success`] / [`record_failure`] — fed by every live `exec`/`ping`
//!     dial and by the 60s `roster_sync` probe (the reused heartbeat).
//!
//! Sustained failure surfaces via [`take_stale_routes`], which the roster tick
//! turns into an operator notification with a suppress-address remediation.
//!
//! [`known_down`] is a third, read-only consumer: the mesh-wide `system
//! health` sweep checks it before dialing a peer, so a peer the heartbeat
//! already knows is unreachable doesn't cost the operator a fresh connect
//! timeout on every call.
//!
//! Cache freshness TTL is 60s — matched to the `roster_sync` tick so a "last
//! good" address stays trusted exactly until the next probe re-confirms it.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// A "last good" address is trusted-first for this long before it must be
/// re-confirmed by a probe. Matched to the roster_sync tick interval.
const TTL_MS: u64 = 60_000;

/// How long an address must be continuously failing (no success since the first
/// failure) before we raise a stale-route notification. ~15 roster ticks.
const SUSTAINED_FAIL_MS: u64 = 900_000;

#[derive(Debug, Clone, Default)]
struct Route {
    /// Epoch-ms of the most recent successful dial, if any.
    last_success_ms: Option<u64>,
    /// Consecutive failed dials since the last success.
    consecutive_failures: u32,
    /// Epoch-ms of the first failure in the current failing streak.
    first_fail_ms: Option<u64>,
    /// Set once we've raised a notification for the current outage, so we don't
    /// re-notify every tick. Cleared on the next success.
    notified: bool,
}

fn cache() -> &'static Mutex<HashMap<(String, String), Route>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), Route>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record a successful dial to `addr` for `peer_id`. Clears the failing streak
/// and any pending stale-route notification flag.
pub fn record_success(peer_id: &str, addr: &str) {
    let mut g = cache().lock().expect("route_health poisoned");
    let r = g
        .entry((peer_id.to_string(), addr.to_string()))
        .or_default();
    r.last_success_ms = Some(now_ms());
    r.consecutive_failures = 0;
    r.first_fail_ms = None;
    r.notified = false;
}

/// Record a failed dial to `addr` for `peer_id`. Starts (or extends) the
/// failing streak.
pub fn record_failure(peer_id: &str, addr: &str) {
    let now = now_ms();
    let mut g = cache().lock().expect("route_health poisoned");
    let r = g
        .entry((peer_id.to_string(), addr.to_string()))
        .or_default();
    r.consecutive_failures = r.consecutive_failures.saturating_add(1);
    r.first_fail_ms.get_or_insert(now);
}

/// Rank an address for `peer_id` — lower sorts earlier:
///   0 = fresh success (last success within TTL) → try first
///   1 = neutral (never dialed, or stale success) → preserve preference order
///   2 = currently failing (failures since last success) → try last
fn rank(g: &HashMap<(String, String), Route>, peer_id: &str, addr: &str, now: u64) -> u8 {
    match g.get(&(peer_id.to_string(), addr.to_string())) {
        Some(r)
            if r.last_success_ms
                .is_some_and(|t| now.saturating_sub(t) <= TTL_MS) =>
        {
            0
        }
        Some(r) if r.consecutive_failures > 0 => 2,
        _ => 1,
    }
}

/// Stable-sort `targets` in place by route health, preserving the caller's
/// original preference order within each rank tier.
pub fn reorder(peer_id: &str, targets: &mut [String]) {
    if targets.len() < 2 {
        return;
    }
    let now = now_ms();
    let g = cache().lock().expect("route_health poisoned");
    // Stable sort keeps the pure preference order (`select_dial_targets`) as the
    // tie-breaker within a rank tier.
    targets.sort_by_key(|addr| rank(&g, peer_id, addr, now));
}

/// A route that has been failing long enough to warrant an operator alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRoute {
    pub addr: String,
    pub minutes_down: u64,
    pub consecutive_failures: u32,
}

/// Return (and mark-notified) any routes for `peer_id` that have been failing
/// continuously past [`SUSTAINED_FAIL_MS`] with no intervening success. Each
/// outage is reported once — the `notified` flag clears on the next success.
pub fn take_stale_routes(peer_id: &str) -> Vec<StaleRoute> {
    let now = now_ms();
    let mut out = Vec::new();
    let mut g = cache().lock().expect("route_health poisoned");
    for ((pid, addr), r) in g.iter_mut() {
        if pid != peer_id || r.notified {
            continue;
        }
        let Some(first_fail) = r.first_fail_ms else {
            continue;
        };
        // A success since the first failure would have reset first_fail_ms, so a
        // present first_fail_ms with no fresher success means a live outage.
        let fresh_success = r.last_success_ms.is_some_and(|s| s >= first_fail);
        if fresh_success {
            continue;
        }
        let down = now.saturating_sub(first_fail);
        if down >= SUSTAINED_FAIL_MS {
            r.notified = true;
            out.push(StaleRoute {
                addr: addr.clone(),
                minutes_down: down / 60_000,
                consecutive_failures: r.consecutive_failures,
            });
        }
    }
    out
}

/// A peer whose every cached address is currently in a failing streak, as
/// reported by [`known_down`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownDown {
    /// Epoch-ms of the earliest ongoing failure across the peer's addresses.
    pub down_since_ms: u64,
    /// The worst (highest) consecutive-failure count across its addresses.
    pub consecutive_failures: u32,
}

/// Outcome of checking one cached [`Route`] against `now`.
enum RouteCheck {
    /// Failing, with the epoch-ms of the first failure in the streak.
    Down(u64, u32),
    /// A fresh success, or no failing streak at all.
    Reachable,
    /// `consecutive_failures > 0` but `first_fail_ms` is unset — `record_failure`
    /// always sets both together, so this should never happen; treat as
    /// "can't tell" rather than letting it blow up the caller.
    Unknown,
}

fn check_route(r: &Route, now: u64) -> RouteCheck {
    let fresh_success = r
        .last_success_ms
        .is_some_and(|t| now.saturating_sub(t) <= TTL_MS);
    if fresh_success || r.consecutive_failures == 0 {
        return RouteCheck::Reachable;
    }
    match r.first_fail_ms {
        Some(first_fail) => RouteCheck::Down(first_fail, r.consecutive_failures),
        None => RouteCheck::Unknown,
    }
}

/// Read-only: is `peer_id` known-down from the route-health cache, with no
/// live dial needed?
///
/// `current_addresses` is the peer's *current* address set (e.g. from
/// [`crate::mesh::dialer::dial_targets_for_peer`]) — the same source the live
/// dial path uses. True only when EVERY address in it has a cache entry that
/// is both failing and has no success within [`TTL_MS`]; an address the peer
/// currently has but that was never dialed (so it has no cache entry at all)
/// means we can't rule it out, so this fails open to `None` rather than
/// reporting the peer down. A fresh success on any one address also means the
/// peer is reachable by at least one route.
///
/// When `current_addresses` is empty — no authoritative address list is
/// available (e.g. the peer has no registered routes at all) — falls back to
/// whatever is cached for the peer, since there's nothing else to check
/// against.
///
/// Side-effect-free: unlike [`take_stale_routes`], this never mutates
/// `notified`, so it is safe to call on every `system health` sweep.
pub fn known_down(peer_id: &str, current_addresses: &[String]) -> Option<KnownDown> {
    let now = now_ms();
    let g = cache().lock().expect("route_health poisoned");
    if current_addresses.is_empty() {
        return known_down_from_cache(&g, peer_id, now);
    }
    let mut down_since_ms: Option<u64> = None;
    let mut consecutive_failures = 0u32;
    for addr in current_addresses {
        let r = g.get(&(peer_id.to_string(), addr.clone()))?;
        match check_route(r, now) {
            RouteCheck::Reachable => return None,
            RouteCheck::Unknown => continue,
            RouteCheck::Down(first_fail, failures) => {
                down_since_ms = Some(down_since_ms.map_or(first_fail, |d| d.min(first_fail)));
                consecutive_failures = consecutive_failures.max(failures);
            }
        }
    }
    down_since_ms.map(|down_since_ms| KnownDown {
        down_since_ms,
        consecutive_failures,
    })
}

/// Cache-only fallback for [`known_down`] when the caller has no current
/// address list to cross-check against.
fn known_down_from_cache(
    g: &HashMap<(String, String), Route>,
    peer_id: &str,
    now: u64,
) -> Option<KnownDown> {
    let mut down_since_ms: Option<u64> = None;
    let mut consecutive_failures = 0u32;
    for ((pid, _), r) in g.iter() {
        if pid != peer_id {
            continue;
        }
        match check_route(r, now) {
            RouteCheck::Reachable => return None,
            RouteCheck::Unknown => continue,
            RouteCheck::Down(first_fail, failures) => {
                down_since_ms = Some(down_since_ms.map_or(first_fail, |d| d.min(first_fail)));
                consecutive_failures = consecutive_failures.max(failures);
            }
        }
    }
    down_since_ms.map(|down_since_ms| KnownDown {
        down_since_ms,
        consecutive_failures,
    })
}

/// Drop every cached route for a peer whose `peer_id` is not in `active`. The
/// cache is otherwise append-only: `record_success`/`record_failure` insert a
/// `(peer_id, address)` on first dial and nothing ever removes it, so a departed
/// or forgotten peer (and any address it's ever been dialed on) would accumulate
/// forever — a slow unbounded-growth leak keyed on peer/address churn. The roster
/// tick calls this once per pass with the current membership so retired peers are
/// reclaimed. Mirrors the `peer_info` liveness cache's `retain_only`.
pub fn retain_peers(active: &HashSet<String>) {
    let mut g = cache().lock().expect("route_health poisoned");
    g.retain(|(pid, _), _| active.contains(pid));
}

/// Forget every cached route for a single peer — for explicit retirement
/// (`system.mesh.delete` / departure) so its entries don't linger until the next
/// membership-driven [`retain_peers`] sweep.
pub fn forget_peer(peer_id: &str) {
    let mut g = cache().lock().expect("route_health poisoned");
    g.retain(|(pid, _), _| pid != peer_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cache is a process-global; use peer ids unique per test to avoid
    // cross-test interference.
    #[test]
    fn success_ranks_before_neutral_before_failing() {
        let p = "peer-rank";
        record_success(p, "good");
        record_failure(p, "bad");
        // "neutral" never dialed.
        let mut targets = vec!["bad".to_string(), "neutral".to_string(), "good".to_string()];
        reorder(p, &mut targets);
        assert_eq!(targets, vec!["good", "neutral", "bad"]);
    }

    #[test]
    fn reorder_is_stable_within_a_tier() {
        let p = "peer-stable";
        // All neutral → original order preserved.
        let mut targets = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        reorder(p, &mut targets);
        assert_eq!(targets, vec!["a", "b", "c"]);
    }

    #[test]
    fn success_clears_failing_streak() {
        let p = "peer-clear";
        record_failure(p, "x");
        record_failure(p, "x");
        record_success(p, "x");
        let mut targets = vec!["x".to_string(), "y".to_string()];
        reorder(p, &mut targets);
        // x now fresh-success → ranks first.
        assert_eq!(targets, vec!["x", "y"]);
        // And no stale route should be pending.
        assert!(take_stale_routes(p).is_empty());
    }

    #[test]
    fn stale_route_reported_once_after_sustained_failure() {
        let p = "peer-stale";
        let addr = "dead".to_string();
        // Seed a failing streak whose first failure is well past the threshold.
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), addr.clone()),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 20,
                    first_fail_ms: Some(now_ms().saturating_sub(SUSTAINED_FAIL_MS + 1000)),
                    notified: false,
                },
            );
        }
        let first = take_stale_routes(p);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].addr, "dead");
        assert!(first[0].minutes_down >= 15);
        // Second call: already notified → nothing.
        assert!(take_stale_routes(p).is_empty());
    }

    #[test]
    fn fresh_failure_not_yet_stale() {
        let p = "peer-fresh-fail";
        record_failure(p, "z");
        // Only just started failing → below sustained threshold.
        assert!(take_stale_routes(p).is_empty());
    }

    #[test]
    fn retain_peers_drops_absent_peers() {
        let keep = "peer-retain-keep";
        let drop = "peer-retain-drop";
        record_failure(keep, "a");
        record_failure(drop, "b");
        let active: HashSet<String> = [keep.to_string()].into_iter().collect();
        retain_peers(&active);
        let g = cache().lock().unwrap();
        assert!(g.contains_key(&(keep.to_string(), "a".to_string())));
        assert!(!g.contains_key(&(drop.to_string(), "b".to_string())));
    }

    #[test]
    fn known_down_is_none_for_a_never_dialed_peer() {
        let p = "peer-known-down-unseen";
        assert_eq!(known_down(p, &[]), None);
    }

    #[test]
    fn known_down_is_none_with_a_recent_success_despite_an_old_failure() {
        let p = "peer-known-down-recent-success";
        let addr = "a".to_string();
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), addr),
                Route {
                    last_success_ms: Some(now_ms()),
                    consecutive_failures: 3,
                    first_fail_ms: Some(now_ms().saturating_sub(SUSTAINED_FAIL_MS)),
                    notified: false,
                },
            );
        }
        assert_eq!(known_down(p, &[]), None);
    }

    #[test]
    fn known_down_is_some_for_a_failing_streak_past_ttl() {
        let p = "peer-known-down-failing";
        let addr = "a".to_string();
        let first_fail = now_ms().saturating_sub(TTL_MS + 5_000);
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), addr),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 4,
                    first_fail_ms: Some(first_fail),
                    notified: false,
                },
            );
        }
        let down = known_down(p, &[]).expect("expected known-down");
        assert_eq!(down.down_since_ms, first_fail);
        assert_eq!(down.consecutive_failures, 4);
    }

    #[test]
    fn known_down_is_some_when_every_current_address_is_cached_and_failing() {
        let p = "peer-known-down-all-current-failing";
        let addr_a = "a".to_string();
        let addr_b = "b".to_string();
        let first_fail = now_ms().saturating_sub(TTL_MS + 5_000);
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), addr_a.clone()),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 4,
                    first_fail_ms: Some(first_fail),
                    notified: false,
                },
            );
            g.insert(
                (p.to_string(), addr_b.clone()),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 2,
                    first_fail_ms: Some(first_fail + 1_000),
                    notified: false,
                },
            );
        }
        let down = known_down(p, &[addr_a, addr_b]).expect("expected known-down");
        assert_eq!(down.down_since_ms, first_fail);
        assert_eq!(down.consecutive_failures, 4);
    }

    #[test]
    fn known_down_is_none_when_a_current_address_has_never_been_dialed() {
        // Reproduces the bug: one stale-failing cached address plus a second
        // current address that has no cache entry at all (never dialed) must
        // fail open to `None`, not report the peer down.
        let p = "peer-known-down-untried-address";
        let stale = "stale".to_string();
        let untried = "untried".to_string();
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), stale.clone()),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 4,
                    first_fail_ms: Some(now_ms().saturating_sub(TTL_MS + 5_000)),
                    notified: false,
                },
            );
        }
        assert_eq!(known_down(p, &[stale, untried]), None);
    }

    #[test]
    fn known_down_skips_a_route_that_violates_the_first_fail_invariant() {
        // `consecutive_failures > 0` with no `first_fail_ms` should never
        // happen (record_failure always sets both together) but must skip
        // the route rather than aborting the whole check.
        let p = "peer-known-down-invariant-violation";
        let addr = "a".to_string();
        {
            let mut g = cache().lock().unwrap();
            g.insert(
                (p.to_string(), addr.clone()),
                Route {
                    last_success_ms: None,
                    consecutive_failures: 4,
                    first_fail_ms: None,
                    notified: false,
                },
            );
        }
        assert_eq!(known_down(p, std::slice::from_ref(&addr)), None);
        assert_eq!(known_down(p, &[]), None);
    }

    #[test]
    fn forget_peer_drops_all_its_routes() {
        let p = "peer-forget";
        record_failure(p, "a");
        record_success(p, "b");
        forget_peer(p);
        let g = cache().lock().unwrap();
        assert!(!g.keys().any(|(pid, _)| pid == p));
    }
}
