//! The process-wide async reactor and its sync→async bridge.

use std::future::Future;

fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build shared reactor")
    })
}

/// Drive `fut` to completion on the shared reactor and return its output. Safe
/// to call from inside a running runtime: the future is then driven from a
/// scoped thread, since a runtime cannot block on itself.
pub fn block_on<F>(fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_err() {
        return shared_runtime().block_on(fut);
    }
    std::thread::scope(|s| {
        s.spawn(|| shared_runtime().block_on(fut))
            .join()
            .unwrap_or_else(|p| std::panic::resume_unwind(p))
    })
}

/// Spawn a fire-and-forget task on the shared reactor.
pub fn spawn_detached<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    drop(shared_runtime().spawn(fut));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_outside_a_runtime() {
        assert_eq!(block_on(async { 7 }), 7);
    }

    #[tokio::test]
    async fn block_on_inside_a_current_thread_runtime() {
        assert_eq!(block_on(async { 8 }), 8);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn block_on_inside_a_multi_thread_runtime() {
        assert_eq!(block_on(async { 9 }), 9);
    }
}
