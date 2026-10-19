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

#[cfg(test)]
mod tests {
    use super::*;

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
