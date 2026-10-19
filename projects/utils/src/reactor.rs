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
/// to call from a multi-thread runtime worker, which is moved out of the
/// runtime for the duration; a current-thread runtime cannot block on itself
/// and panics.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| shared_runtime().block_on(fut))
        }
        _ => shared_runtime().block_on(fut),
    }
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

    #[test]
    fn block_on_accepts_non_send_futures() {
        let rc = std::rc::Rc::new(8);
        assert_eq!(block_on(async move { *rc }), 8);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn block_on_inside_a_multi_thread_runtime() {
        assert_eq!(block_on(async { 9 }), 9);
    }
}
