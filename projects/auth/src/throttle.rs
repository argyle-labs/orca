//! In-memory signin failure throttle.
//!
//! Argon2id is cheap on LAN (~50 ms per verify), so naive brute-force is
//! feasible without rate limits. This module records failed signin attempts
//! keyed by (caller_ip, username_lower) and refuses further attempts once a
//! threshold within a sliding window is reached.
//!
//! Process-local — survives no daemon restart. That's fine: a restart costs
//! the attacker the seconds of cargo-watch reload, and a real DB-backed
//! lockout would need careful TTL semantics + replay protection. The point
//! here is to defeat sustained online guessing, not a determined attacker
//! with shell access.
//!
//! Also tracked per-IP-alone and per-user-alone so an attacker can't
//! evade by varying one axis.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window in which failures accumulate. Older entries are pruned on each call.
const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Max failures per key within `WINDOW` before signin is refused.
const MAX_FAILURES: usize = 8;

#[derive(Default)]
struct Bucket {
    /// Failure timestamps within the window. Sorted ascending.
    failures: Vec<Instant>,
}

impl Bucket {
    fn record_failure(&mut self, now: Instant) {
        self.prune(now);
        self.failures.push(now);
    }

    fn prune(&mut self, now: Instant) {
        let cutoff = now.checked_sub(WINDOW).unwrap_or(now);
        self.failures.retain(|t| *t >= cutoff);
    }

    fn count(&mut self, now: Instant) -> usize {
        self.prune(now);
        self.failures.len()
    }
}

#[derive(Default)]
struct State {
    by_ip_user: HashMap<(String, String), Bucket>,
    by_ip: HashMap<String, Bucket>,
    by_user: HashMap<String, Bucket>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().expect("auth_throttle mutex poisoned");
    let s = guard.get_or_insert_with(State::default);
    f(s)
}

/// Outcome of a pre-check. `Allowed` means the caller may attempt signin;
/// `Throttled` carries the seconds to wait before retrying.
#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Allowed,
    Throttled { retry_after_secs: u64 },
}

/// Test seam: makes `now` injectable so the unit tests don't sleep.
pub(crate) fn check_at(ip: &str, username: &str, now: Instant) -> CheckOutcome {
    let user_key = username.to_lowercase();
    with_state(|s| {
        let ip_user = s
            .by_ip_user
            .entry((ip.to_string(), user_key.clone()))
            .or_default()
            .count(now);
        let ip_only = s.by_ip.entry(ip.to_string()).or_default().count(now);
        let user_only = s.by_user.entry(user_key.clone()).or_default().count(now);
        // Pruning during count() drains expired entries but leaves empty
        // buckets in the map. Scanners that hit many (ip, username) pairs
        // would otherwise accumulate permanent empty entries.
        s.by_ip_user.retain(|_, b| !b.failures.is_empty());
        s.by_ip.retain(|_, b| !b.failures.is_empty());
        s.by_user.retain(|_, b| !b.failures.is_empty());
        let worst = ip_user.max(ip_only).max(user_only);
        if worst >= MAX_FAILURES {
            CheckOutcome::Throttled {
                retry_after_secs: WINDOW.as_secs(),
            }
        } else {
            CheckOutcome::Allowed
        }
    })
}

pub fn check(ip: &str, username: &str) -> CheckOutcome {
    check_at(ip, username, Instant::now())
}

pub(crate) fn record_failure_at(ip: &str, username: &str, now: Instant) {
    let user_key = username.to_lowercase();
    with_state(|s| {
        s.by_ip_user
            .entry((ip.to_string(), user_key.clone()))
            .or_default()
            .record_failure(now);
        s.by_ip
            .entry(ip.to_string())
            .or_default()
            .record_failure(now);
        s.by_user.entry(user_key).or_default().record_failure(now);
    });
}

pub fn record_failure(ip: &str, username: &str) {
    record_failure_at(ip, username, Instant::now())
}

pub fn record_success(ip: &str, username: &str) {
    let user_key = username.to_lowercase();
    with_state(|s| {
        s.by_ip_user.remove(&(ip.to_string(), user_key.clone()));
        s.by_ip.remove(ip);
        s.by_user.remove(&user_key);
    });
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    with_state(|s| {
        s.by_ip_user.clear();
        s.by_ip.clear();
        s.by_user.clear();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Tests in this module share the process-wide `STATE` mutex, so we
    /// serialise them through a dedicated test lock — each test resets first,
    /// then operates without interleaving.
    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn allows_below_threshold_and_throttles_at_threshold() {
        let _g = test_guard();
        reset_for_tests();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES - 1 {
            record_failure_at("1.1.1.1", "alice", now);
        }
        assert_eq!(check_at("1.1.1.1", "alice", now), CheckOutcome::Allowed);
        record_failure_at("1.1.1.1", "alice", now);
        assert!(matches!(
            check_at("1.1.1.1", "alice", now),
            CheckOutcome::Throttled { .. }
        ));
    }

    #[test]
    fn ip_throttle_holds_across_usernames() {
        let _g = test_guard();
        reset_for_tests();
        let now = Instant::now();
        for i in 0..MAX_FAILURES {
            record_failure_at("2.2.2.2", &format!("u{i}"), now);
        }
        // Same IP, totally fresh username should still be throttled.
        assert!(matches!(
            check_at("2.2.2.2", "new-user", now),
            CheckOutcome::Throttled { .. }
        ));
    }

    #[test]
    fn user_throttle_holds_across_ips() {
        let _g = test_guard();
        reset_for_tests();
        let now = Instant::now();
        for i in 0..MAX_FAILURES {
            record_failure_at(&format!("3.3.3.{i}"), "bob", now);
        }
        assert!(matches!(
            check_at("9.9.9.9", "bob", now),
            CheckOutcome::Throttled { .. }
        ));
    }

    #[test]
    fn record_success_clears_buckets() {
        let _g = test_guard();
        reset_for_tests();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES {
            record_failure_at("4.4.4.4", "carol", now);
        }
        assert!(matches!(
            check_at("4.4.4.4", "carol", now),
            CheckOutcome::Throttled { .. }
        ));
        record_success("4.4.4.4", "carol");
        assert_eq!(check_at("4.4.4.4", "carol", now), CheckOutcome::Allowed);
    }

    #[test]
    fn window_expiry_drops_old_failures() {
        let _g = test_guard();
        reset_for_tests();
        let old = Instant::now()
            .checked_sub(WINDOW + Duration::from_secs(60))
            .expect("test wall-clock supports rewind");
        for _ in 0..MAX_FAILURES {
            record_failure_at("5.5.5.5", "dave", old);
        }
        // Far in the future, the old failures should be pruned out.
        let now = old + WINDOW + Duration::from_secs(120);
        assert_eq!(check_at("5.5.5.5", "dave", now), CheckOutcome::Allowed);
    }

    #[test]
    fn username_match_is_case_insensitive() {
        let _g = test_guard();
        reset_for_tests();
        let now = Instant::now();
        for _ in 0..MAX_FAILURES {
            record_failure_at("6.6.6.6", "Eve", now);
        }
        assert!(matches!(
            check_at("6.6.6.6", "eve", now),
            CheckOutcome::Throttled { .. }
        ));
    }

    #[test]
    fn public_wrappers_delegate_correctly() {
        let _g = test_guard();
        reset_for_tests();
        // record_failure delegates to record_failure_at
        for _ in 0..MAX_FAILURES {
            record_failure("7.7.7.7", "frank");
        }
        // check delegates to check_at
        assert!(matches!(
            check("7.7.7.7", "frank"),
            CheckOutcome::Throttled { .. }
        ));
    }
}
