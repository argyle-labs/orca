use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(dur: Duration) -> Self {
        let now = now_instant();
        Self(
            now.checked_add(dur)
                .unwrap_or_else(|| now + Duration::from_secs(100 * 365 * 24 * 60 * 60)),
        )
    }

    pub fn reached(&self) -> bool {
        now_instant() >= self.0
    }

    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(now_instant())
    }
}

fn now_instant() -> Instant {
    tokio::time::Instant::now().into_std()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe<T> {
    Settled(T),
    Pending(T),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Polled<T> {
    Settled(T),
    Exhausted(T),
    Expired(T),
}

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
        tokio::time::sleep(interval.min(deadline.remaining())).await;
        if deadline.reached() {
            return Ok(Polled::Expired(value));
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn deadline_reached_after_elapse() {
        let d = Deadline::after(Duration::from_millis(5));
        assert!(!d.reached());
        tokio::time::sleep(Duration::from_millis(15)).await;
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
    fn deadline_after_an_unrepresentable_duration_saturates() {
        let d = Deadline::after(Duration::MAX);
        assert!(!d.reached());
        assert!(d.remaining() > Duration::from_secs(3600));
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
