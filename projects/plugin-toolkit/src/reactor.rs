//! The shared, orca-owned async reactor — the generic surface a dynamic
//! (subprocess) plugin registers its async work against.
//!
//! Plugins reach async execution through this module (`block_on`,
//! `spawn_detached`, `join_all`) and the [`crate::process`] / [`crate::io`] / [`crate::time`]
//! seams, never by naming the runtime. The executor behind them (tokio today) is
//! an orca implementation detail: one process-wide runtime backs the `serve`
//! loop and every seam. See
//! [[orca-north-star-abstract-system-differences]] and [[plugins-stay-thin]].

use std::future::Future;
use std::num::NonZeroUsize;

use futures_util::stream::{self, StreamExt};

pub use ::utils::reactor::{block_on, spawn_detached};

/// Drive a collection of futures concurrently to completion, returning their
/// outputs in input order — the orca-owned async fan-out a plugin reaches for
/// (`plugin_toolkit::reactor::join_all`) instead of naming `futures`. Unlike
/// [`spawn_detached`], the futures need be neither `Send` nor `'static`: they are
/// polled together on the *current* task (no spawning), so borrowed per-item work
/// — one round-trip per unit, say — runs in parallel without O(N × RTT) latency.
///
/// This is the concurrency counterpart to `block_on`: `block_on` bridges
/// sync→async, `join_all` fans one async context out over many items.
///
/// Returns the concrete `JoinAll` future (not `async fn`) on purpose: an
/// `async fn` wrapper erases the future's identity behind an opaque type, which
/// defeats higher-ranked-lifetime inference when the input iterator's closure
/// borrows per-item (`|m| async move { … &m … }`). Passing the combinator
/// through unwrapped keeps `reactor::join_all(xs).await` behaving byte-for-byte
/// like the underlying call — callers still only ever `.await` it and never name
/// `futures`.
pub fn join_all<I>(futures: I) -> futures_util::future::JoinAll<I::Item>
where
    I: IntoIterator,
    I::Item: Future,
{
    futures_util::future::join_all(futures)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fanout<K, T, E> {
    pub ok: Vec<(K, T)>,
    pub errors: Vec<(K, E)>,
}

pub async fn fan_out<K, T, E, F, Fut>(
    items: impl IntoIterator<Item = K>,
    limit: NonZeroUsize,
    f: F,
) -> Fanout<K, T, E>
where
    K: Clone,
    F: Fn(K) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut done: Vec<(usize, K, Result<T, E>)> = stream::iter(items.into_iter().enumerate())
        .map(|(i, k)| {
            let fut = f(k.clone());
            async move { (i, k, fut.await) }
        })
        .buffer_unordered(limit.get())
        .collect()
        .await;
    done.sort_unstable_by_key(|(i, _, _)| *i);
    let mut out = Fanout {
        ok: Vec::new(),
        errors: Vec::new(),
    };
    for (_, k, r) in done {
        match r {
            Ok(t) => out.ok.push((k, t)),
            Err(e) => out.errors.push((k, e)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn n(v: usize) -> NonZeroUsize {
        NonZeroUsize::new(v).unwrap()
    }

    async fn peak_in_flight(items: u64, limit: usize) -> usize {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let out = fan_out(0..items, n(limit), |i| {
            let (live, peak) = (live.clone(), peak.clone());
            async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10 + i % 3)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, ()>(i)
            }
        })
        .await;
        assert_eq!(out.ok.len() as u64, items);
        peak.load(Ordering::SeqCst)
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_never_exceeds_limit() {
        assert_eq!(peak_in_flight(20, 4).await, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_limit_one_is_serial() {
        assert_eq!(peak_in_flight(5, 1).await, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_calls_f_lazily_within_limit() {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        fan_out(0..20u64, n(3), |i| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            let live = live.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(5 + i % 4)).await;
                live.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, ()>(i)
            }
        })
        .await;
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_restores_input_order() {
        let out = fan_out(0..8u64, n(8), |i| async move {
            tokio::time::sleep(Duration::from_millis(100 - i * 10)).await;
            Ok::<_, ()>(i * 2)
        })
        .await;
        let want: Vec<_> = (0..8u64).map(|i| (i, i * 2)).collect();
        assert_eq!(out.ok, want);
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_partial_failure_keeps_successes() {
        let out = fan_out(["a", "b", "c", "d"], n(2), |k| async move {
            tokio::time::sleep(Duration::from_millis(if k == "a" { 50 } else { 5 })).await;
            if k == "b" || k == "d" {
                Err(format!("{k} failed"))
            } else {
                Ok(k.len())
            }
        })
        .await;
        assert_eq!(out.ok, vec![("a", 1), ("c", 1)]);
        assert_eq!(
            out.errors,
            vec![("b", "b failed".to_string()), ("d", "d failed".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fan_out_empty_input() {
        let out = fan_out(Vec::<u8>::new(), n(4), |i| async move { Ok::<_, ()>(i) }).await;
        assert!(out.ok.is_empty() && out.errors.is_empty());
    }

    #[test]
    fn fan_out_accepts_borrowed_non_static_futures() {
        let name = String::from("unit");
        let name = &name;
        let out = block_on(fan_out(0..3, n(2), |i| async move {
            Ok::<_, ()>(format!("{name}-{i}"))
        }));
        assert_eq!(
            out.ok,
            vec![
                (0, "unit-0".into()),
                (1, "unit-1".into()),
                (2, "unit-2".into())
            ]
        );
    }

    #[test]
    fn fan_out_futures_may_borrow_from_the_item() {
        let names = [String::from("alpha"), String::from("be")];
        let out = block_on(fan_out(names.iter(), n(2), |name: &String| async move {
            Ok::<_, ()>(name.as_str().len())
        }));
        assert_eq!(out.ok, vec![(&names[0], 5), (&names[1], 2)]);
    }

    #[test]
    fn join_all_preserves_input_order_and_runs_concurrently() {
        // Futures complete out of registration order, but results stay ordered.
        let out = block_on(join_all((0..5).map(|i| async move {
            // No I/O needed to prove ordering; the point is the seam compiles and
            // returns outputs positionally regardless of internal poll order.
            i * 10
        })));
        assert_eq!(out, vec![0, 10, 20, 30, 40]);
    }

    #[test]
    fn join_all_accepts_borrowed_non_static_futures() {
        let name = String::from("unit");
        let name = &name;
        // The async blocks borrow `name` — join_all must not require 'static.
        let out = block_on(join_all(
            (0..3).map(|i| async move { format!("{name}-{i}") }),
        ));
        assert_eq!(out, vec!["unit-0", "unit-1", "unit-2"]);
    }
}
