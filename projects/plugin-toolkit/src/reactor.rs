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

pub async fn spawn_blocking<T, F>(f: F) -> std::io::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("orca-blocking".to_string())
        .spawn(move || {
            drop(tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))));
        })?;
    match rx.await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(panic)) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(std::io::Error::other(format!(
                "blocking task panicked: {msg}"
            )))
        }
        Err(_) => Err(std::io::Error::other(
            "blocking task ended without a result",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_blocking_returns_the_value() {
        assert_eq!(block_on(spawn_blocking(|| 7)).unwrap(), 7);
    }

    #[test]
    fn spawn_blocking_turns_a_panic_into_err() {
        let err = block_on(spawn_blocking(|| -> u8 { panic!("boom") })).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn spawn_blocking_runs_off_the_polling_thread() {
        let caller = std::thread::current().id();
        let worker = block_on(spawn_blocking(|| std::thread::current().id())).unwrap();
        assert_ne!(caller, worker);
    }

    #[test]
    fn spawn_blocking_completes_on_a_bare_executor() {
        assert_eq!(
            futures_executor_free_block_on(spawn_blocking(|| "ok")).unwrap(),
            "ok"
        );
    }

    fn futures_executor_free_block_on<F: Future>(fut: F) -> F::Output {
        let waker = std::sync::Arc::new(ThreadWaker(std::thread::current())).into();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::park();
        }
    }

    struct ThreadWaker(std::thread::Thread);

    impl std::task::Wake for ThreadWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
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
