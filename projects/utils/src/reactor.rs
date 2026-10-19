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

pub fn block_on<F: Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| shared_runtime().block_on(fut))
        }
        _ => shared_runtime().block_on(fut),
    }
}

pub fn off_async_worker<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        Ok(_) => std::thread::scope(|s| {
            s.spawn(f)
                .join()
                .unwrap_or_else(|p| std::panic::resume_unwind(p))
        }),
        Err(_) => f(),
    }
}

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

    #[tokio::test(flavor = "multi_thread")]
    async fn block_on_inside_a_multi_thread_runtime() {
        assert_eq!(block_on(async { 9 }), 9);
    }

    #[test]
    fn block_on_accepts_non_send_futures() {
        let rc = std::rc::Rc::new(8);
        assert_eq!(block_on(async move { *rc }), 8);
    }

    fn nested_block_on() -> i32 {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { 7 })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn off_async_worker_leaves_a_multi_thread_worker() {
        assert_eq!(off_async_worker(nested_block_on), 7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn off_async_worker_leaves_a_current_thread_runtime() {
        assert_eq!(off_async_worker(nested_block_on), 7);
    }

    #[test]
    fn off_async_worker_runs_inline_without_a_runtime() {
        let caller = std::thread::current().id();
        assert_eq!(off_async_worker(|| std::thread::current().id()), caller);
    }
}
