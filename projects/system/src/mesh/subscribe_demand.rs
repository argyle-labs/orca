//! Adaptive-cadence demand signal for the `pod/subscribe` bus (slice D).
//!
//! Each daemon tracks a single "last heartbeat seen" timestamp. Any
//! subscriber session that has sent a heartbeat within the demand window
//! is treated as live demand, and the local writer bumps its emit cadence
//! to near-realtime. Demand falls back to background cadence as soon as
//! the most recent heartbeat ages past the window.
//!
//! The simplification is intentional: per-session tracking would let one
//! still-alive session keep the cadence fast even if all others dropped,
//! but that's actually the correct behavior — that one session IS
//! watching, so we should keep up. A single global timestamp captures
//! "is anyone watching?" without the bookkeeping of a session table.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

/// Maximum age of the most recent heartbeat for demand to count as live.
/// Sized to comfortably outlive 1–2 heartbeats at the client-side cadence
/// (5s) without being so generous that a dead session keeps cadence fast.
pub const DEMAND_WINDOW: Duration = Duration::from_secs(15);

/// Fast emit cadence used when demand is live. Memo target is "1–5s".
pub const FAST_CADENCE: Duration = Duration::from_secs(2);

/// Background emit cadence used when nobody is watching. Memo target is
/// "every 30s — enough for retention/history".
pub const SLOW_CADENCE: Duration = Duration::from_secs(30);

static LAST_HEARTBEAT_UNIX: AtomicI64 = AtomicI64::new(0);
static HEARTBEATS_SEEN: AtomicU64 = AtomicU64::new(0);

/// Record a heartbeat at the current wall-clock time. Bumps the
/// global "anyone watching?" timestamp AND a monotonic counter; the
/// counter is useful for tests that need to assert progress without
/// racing other concurrent tests.
pub fn touch() {
    touch_at(utils::time::now().unix_seconds());
}

/// Test seam — set the registry to a specific Unix timestamp.
fn touch_at(unix: i64) {
    LAST_HEARTBEAT_UNIX.store(unix, Ordering::Relaxed);
    HEARTBEATS_SEEN.fetch_add(1, Ordering::Relaxed);
}

/// Monotonic count of heartbeats observed since process start.
pub fn heartbeats_seen() -> u64 {
    HEARTBEATS_SEEN.load(Ordering::Relaxed)
}

/// Return true iff the most recent heartbeat is no older than `window`
/// relative to `now_unix`. Pure — no I/O — so cadence choices are
/// deterministic in tests.
pub fn is_live_at(now_unix: i64, window: Duration) -> bool {
    let last = LAST_HEARTBEAT_UNIX.load(Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let age = now_unix.saturating_sub(last);
    age >= 0 && (age as u64) <= window.as_secs()
}

/// Snapshot the current "is anyone watching me?" answer.
pub fn is_live() -> bool {
    is_live_at(utils::time::now().unix_seconds(), DEMAND_WINDOW)
}

/// Choose the emit cadence given a demand signal. Pulled out so the
/// publisher's tick loop is a one-liner.
pub fn choose_cadence(has_demand: bool, fast: Duration, slow: Duration) -> Duration {
    if has_demand { fast } else { slow }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests mutate the global; serialize them so concurrent races don't
    /// flip the timestamp under our feet.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn reset_registry() {
        LAST_HEARTBEAT_UNIX.store(0, Ordering::Relaxed);
    }

    #[test]
    fn cold_registry_reports_no_demand() {
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        assert!(!is_live_at(1_000, DEMAND_WINDOW));
    }

    #[test]
    fn heartbeat_within_window_is_live() {
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        touch_at(1_000);
        assert!(is_live_at(1_005, Duration::from_secs(15)));
    }

    #[test]
    fn heartbeat_at_exact_window_is_still_live() {
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        touch_at(1_000);
        assert!(is_live_at(1_015, Duration::from_secs(15)));
    }

    #[test]
    fn heartbeat_past_window_is_not_live() {
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        touch_at(1_000);
        assert!(!is_live_at(1_016, Duration::from_secs(15)));
    }

    #[test]
    fn future_heartbeat_is_treated_as_not_live() {
        // Defensive: a clock skew that produces "last > now" shouldn't
        // wedge the publisher into permanent fast mode.
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        touch_at(2_000);
        assert!(!is_live_at(1_000, Duration::from_secs(15)));
    }

    #[test]
    fn touch_uses_wall_clock_so_is_live_is_true_immediately_after() {
        let _g = SERIAL.lock().unwrap();
        reset_registry();
        touch();
        assert!(is_live());
    }

    #[test]
    fn choose_cadence_prefers_fast_when_demand_live() {
        assert_eq!(
            choose_cadence(true, Duration::from_secs(2), Duration::from_secs(30)),
            Duration::from_secs(2),
        );
    }

    #[test]
    fn choose_cadence_falls_back_to_slow_when_no_demand() {
        assert_eq!(
            choose_cadence(false, Duration::from_secs(2), Duration::from_secs(30)),
            Duration::from_secs(30),
        );
    }
}
