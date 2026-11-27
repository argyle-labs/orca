//! Async time for plugins. The executor stays hidden: deadlines go through
//! [`Deadline`], never a runtime `Instant`.

use std::future::Future;
use std::time::{Duration, Instant};

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
        Self(now_instant() + dur)
    }

    /// True once the current time is at or past the deadline.
    pub fn reached(&self) -> bool {
        now_instant() >= self.0
    }

    /// Time left until the deadline; zero once reached.
    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(now_instant())
    }
}

/// The executor's clock, so paused time in tests drives deadlines and sleeps alike.
fn now_instant() -> Instant {
    tokio::time::Instant::now().into_std()
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
/// probes have run, or `deadline` is reached, sleeping `interval`
/// between probes. At least one probe always runs; a probe error ends the poll.
/// A sleep never runs past the deadline, and no probe starts once it is reached.
pub async fn poll_until<T, E, F, Fut>(
    interval: Duration,
    attempts: u32,
    deadline: &Deadline,
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
        if deadline.reached() {
            return Ok(Polled::Expired(value));
        }
        sleep(interval.min(deadline.remaining())).await;
        if deadline.reached() {
            return Ok(Polled::Expired(value));
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn timeout_some_when_fast() {
        assert_eq!(timeout(Duration::from_secs(5), async { 7 }).await, Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_none_when_slow() {
        let out = timeout(Duration::from_millis(5), async {
            sleep(Duration::from_secs(30)).await;
            7
        })
        .await;
        assert_eq!(out, None);
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_reached_after_elapse() {
        let d = Deadline::after(Duration::from_millis(5));
        assert!(!d.reached());
        sleep(Duration::from_millis(15)).await;
        assert!(d.reached());
    }

    #[test]
    fn deadline_works_outside_a_runtime() {
        let d = Deadline::after(Duration::from_secs(3600));
        assert!(!d.reached());
        assert!(d.remaining() > Duration::ZERO);
        assert!(Deadline::after(Duration::ZERO).reached());
    }

    #[test]
    fn poll_until_future_is_send() {
        fn assert_send<T: Send>(_: T) {}
        let d = Deadline::after(Duration::ZERO);
        assert_send(poll_until(Duration::ZERO, 1, &d, |n| async move {
            Ok::<_, ()>(Probe::Pending(n))
        }));
    }

    fn never() -> Deadline {
        Deadline::after(Duration::from_secs(3600))
    }

    #[tokio::test(start_paused = true)]
    async fn poll_settles_on_first_settled_probe() {
        let mut calls = 0;
        let out: Result<_, ()> = poll_until(Duration::from_secs(5), 10, &never(), |n| {
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
        let out: Result<_, ()> = poll_until(Duration::from_secs(5), 3, &never(), |n| async move {
            Ok(Probe::Pending(n))
        })
        .await;
        assert_eq!(out, Ok(Polled::Exhausted(3)));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_zero_attempts_still_probes_once() {
        let out: Result<_, ()> = poll_until(Duration::from_secs(5), 0, &never(), |n| async move {
            Ok(Probe::Pending(n))
        })
        .await;
        assert_eq!(out, Ok(Polled::Exhausted(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_expires_after_first_probe_when_already_past() {
        let deadline = Deadline::after(Duration::from_secs(1));
        tokio::time::advance(Duration::from_secs(2)).await;
        let start = tokio::time::Instant::now();
        let out: Result<_, ()> =
            poll_until(Duration::from_secs(5), 60, &deadline, |n| async move {
                Ok(Probe::Pending(n))
            })
            .await;
        assert_eq!(out, Ok(Polled::Expired(1)));
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_caps_sleep_at_deadline() {
        let start = tokio::time::Instant::now();
        let deadline = Deadline::after(Duration::from_secs(12));
        let out: Result<_, ()> =
            poll_until(Duration::from_secs(5), 60, &deadline, |n| async move {
                Ok(Probe::Pending(n))
            })
            .await;
        assert_eq!(out, Ok(Polled::Expired(3)));
        assert_eq!(start.elapsed(), Duration::from_secs(12));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_skips_probe_when_deadline_lands_on_interval() {
        let start = tokio::time::Instant::now();
        let deadline = Deadline::after(Duration::from_secs(10));
        let mut calls = 0;
        let out: Result<_, ()> = poll_until(Duration::from_secs(5), 60, &deadline, |n| {
            calls += 1;
            async move { Ok(Probe::Pending(n)) }
        })
        .await;
        assert_eq!(out, Ok(Polled::Expired(2)));
        assert_eq!(calls, 2);
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn remaining_counts_down_to_zero() {
        let d = Deadline::after(Duration::from_secs(10));
        assert_eq!(d.remaining(), Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(d.remaining(), Duration::from_secs(6));
        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(d.remaining(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_settled_wins_over_expired_deadline() {
        let deadline = Deadline::after(Duration::ZERO);
        let out: Result<_, ()> =
            poll_until(Duration::from_secs(5), 60, &deadline, |n| async move {
                Ok(Probe::Settled(n))
            })
            .await;
        assert_eq!(out, Ok(Polled::Settled(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn poll_stops_on_probe_error() {
        let out = poll_until(Duration::from_secs(5), 60, &never(), |n| async move {
            if n == 2 {
                Err("boom")
            } else {
                Ok(Probe::Pending(n))
            }
        })
        .await;
        assert_eq!(out, Err("boom"));
    }
}
