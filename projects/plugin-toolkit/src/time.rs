//! Async time — an orca abstraction, not a runtime re-export.
//!
//! Plugins stay `async` but reach timing through this orca-owned surface, so no
//! executor type or crate name ever appears in plugin code. The executor behind
//! these functions (tokio today) is an orca implementation detail: swap it and
//! no plugin changes. Per [[orca-north-star-abstract-system-differences]] and
//! [[plugins-stay-thin]] — the plugin depends on `plugin_toolkit`, never on the
//! runtime.
//!
//! `Duration` is std vocabulary (like `String`), not a runtime dependency, so it
//! is used directly at the boundary; every other type here is orca-owned. There
//! is deliberately no way to obtain the executor's clock/`Instant` — use
//! [`Deadline`].

use std::future::Future;
use std::time::{Duration, Instant};

// Wall-clock time (an orca-owned instant, chrono hidden) is the `utils::time`
// utility. Re-exposed here so a plugin reaches both async time and wall-clock
// time through one `time` module and never names the datetime library. Gated on
// `tools` (which pulls light, tree-shaken `utils` — no http/tls/git), so any
// tool plugin has it.
#[cfg(feature = "tools")]
pub use ::utils::time::{Timestamp, now};

/// Suspend the current task for `dur` without blocking a thread.
pub async fn sleep(dur: Duration) {
    tokio::time::sleep(dur).await
}

/// Await `fut` with a deadline. `Some(output)` if it finished within `dur`,
/// `None` if it timed out. The executor's timeout-error type never surfaces.
pub async fn timeout<F: Future>(dur: Duration, fut: F) -> Option<F::Output> {
    tokio::time::timeout(dur, fut).await.ok()
}

/// A monotonic point in the future. orca-owned so plugins express "have we run
/// past a budget?" without naming a runtime clock. Construct with
/// [`Deadline::after`], poll with [`Deadline::reached`].
#[derive(Clone, Copy, Debug)]
pub struct Deadline(Instant);

impl Deadline {
    /// A deadline `dur` from now.
    pub fn after(dur: Duration) -> Self {
        Self::after_on(&MonotonicClock, dur)
    }

    /// A deadline `dur` past `clock`'s now.
    pub fn after_on(clock: &dyn Clock, dur: Duration) -> Self {
        Self(clock.now() + dur)
    }

    /// True once the current time is at or past the deadline.
    pub fn reached(&self) -> bool {
        self.reached_on(&MonotonicClock)
    }

    /// True once `clock` is at or past the deadline.
    pub fn reached_on(&self, clock: &dyn Clock) -> bool {
        clock.now() >= self.0
    }
}

/// A monotonic time source, injectable so callers and tests control when a
/// [`Deadline`] is reached.
pub trait Clock: Sync {
    fn now(&self) -> Instant;
}

/// The executor's monotonic clock; follows paused/advanced time under test.
#[derive(Clone, Copy, Debug, Default)]
pub struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }
}

/// One probe's verdict for [`poll_until`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe<T> {
    Settled(T),
    Pending(T),
}

/// How [`poll_until`] ended; every variant carries the last probed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Polled<T> {
    Settled(T),
    /// `attempts` probes ran without settling.
    Exhausted(T),
    /// The deadline was reached before settling or exhausting attempts.
    Expired(T),
}

/// Run `probe` (given the 1-based attempt number) until it settles, `attempts`
/// probes have run, or `deadline` is reached on `clock`, sleeping `interval`
/// between probes. At least one probe always runs; a probe error ends the poll.
pub async fn poll_until<T, E, F, Fut>(
    interval: Duration,
    attempts: u32,
    deadline: &Deadline,
    clock: &dyn Clock,
    mut probe: F,
) -> Result<Polled<T>, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<Probe<T>, E>>,
{
    let attempts = attempts.max(1);
    let mut attempt = 1;
    loop {
        let value = match probe(attempt).await? {
            Probe::Settled(v) => return Ok(Polled::Settled(v)),
            Probe::Pending(v) => v,
        };
        if attempt >= attempts {
            return Ok(Polled::Exhausted(value));
        }
        if deadline.reached_on(clock) {
            return Ok(Polled::Expired(value));
        }
        attempt += 1;
        sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_some_when_fast() {
        assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Some(7));
    }

    #[tokio::test]
    async fn timeout_none_when_slow() {
        let out = timeout(Duration::from_millis(5), async {
            sleep(Duration::from_secs(30)).await;
            7
        })
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn deadline_reached_after_elapse() {
        let d = Deadline::after(Duration::from_millis(5));
        assert!(!d.reached());
        sleep(Duration::from_millis(15)).await;
        assert!(d.reached());
    }

    struct Fixed(Instant);

    impl Clock for Fixed {
        fn now(&self) -> Instant {
            self.0
        }
    }

    fn never() -> Deadline {
        Deadline::after(Duration::from_secs(3600))
    }

    #[tokio::test(start_paused = true)]
    async fn poll_settles_on_first_settled_probe() {
        let mut calls = 0;
        let out: Result<_, ()> =
            poll_until(Duration::from_secs(5), 10, &never(), &MonotonicClock, |n| {
                calls += 1;
                async move {
                    Ok(if n == 3 {
                        Probe::Settled(n)
                    } else {
                        Probe::Pending(n)
                    })
                }
            })
            .await;
        assert_eq!(out, Ok(Polled::Settled(3)));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_exhausts_and_paces_between_probes() {
        let start = tokio::time::Instant::now();
        let out: Result<_, ()> = poll_until(
            Duration::from_secs(5),
            3,
            &never(),
            &MonotonicClock,
            |n| async move { Ok(Probe::Pending(n)) },
        )
        .await;
        assert_eq!(out, Ok(Polled::Exhausted(3)));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_zero_attempts_still_probes_once() {
        let out: Result<_, ()> = poll_until(
            Duration::from_secs(5),
            0,
            &never(),
            &MonotonicClock,
            |n| async move { Ok(Probe::Pending(n)) },
        )
        .await;
        assert_eq!(out, Ok(Polled::Exhausted(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_expires_on_injected_clock_without_sleeping() {
        let now = Instant::now();
        let deadline = Deadline::after_on(&Fixed(now), Duration::from_secs(1));
        let late = Fixed(now + Duration::from_secs(2));
        let start = tokio::time::Instant::now();
        let out: Result<_, ()> = poll_until(
            Duration::from_secs(5),
            60,
            &deadline,
            &late,
            |n| async move { Ok(Probe::Pending(n)) },
        )
        .await;
        assert_eq!(out, Ok(Polled::Expired(1)));
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_expires_when_paused_time_passes_deadline() {
        let deadline = Deadline::after(Duration::from_secs(12));
        let out: Result<_, ()> = poll_until(
            Duration::from_secs(5),
            60,
            &deadline,
            &MonotonicClock,
            |n| async move { Ok(Probe::Pending(n)) },
        )
        .await;
        assert_eq!(out, Ok(Polled::Expired(4)));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_settled_wins_over_expired_deadline() {
        let now = Instant::now();
        let deadline = Deadline::after_on(&Fixed(now), Duration::ZERO);
        let out: Result<_, ()> = poll_until(
            Duration::from_secs(5),
            60,
            &deadline,
            &Fixed(now),
            |n| async move { Ok(Probe::Settled(n)) },
        )
        .await;
        assert_eq!(out, Ok(Polled::Settled(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_stops_on_probe_error() {
        let out = poll_until(
            Duration::from_secs(5),
            60,
            &never(),
            &MonotonicClock,
            |n| async move {
                if n == 2 {
                    Err("boom")
                } else {
                    Ok(Probe::Pending(n))
                }
            },
        )
        .await;
        assert_eq!(out, Err("boom"));
    }
}
