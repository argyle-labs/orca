//! The shared, orca-owned async reactor — the generic surface a dynamic
//! (subprocess) plugin registers its async work against.
//!
//! Plugins reach async execution through this module (`block_on`,
//! `spawn_detached`) and the [`crate::process`] / [`crate::io`] / [`crate::time`]
//! seams, never by naming the runtime. The executor behind them (tokio today) is
//! an orca implementation detail: one process-wide runtime backs the `serve`
//! loop, every seam, and (for legacy cdylibs) `export::runtime`. See
//! [[orca-north-star-abstract-system-differences]] and [[plugins-stay-thin]].

use std::future::Future;

/// The process-wide reactor. Built once, kept for the process lifetime.
/// `pub(crate)` — plugins touch it only through the seams, never as a runtime.
pub(crate) fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build shared plugin reactor")
    })
}

/// Drive `fut` to completion on the shared reactor and return its output — the
/// sync→async bridge a plugin's synchronous `backend_dispatch` (or a test) uses
/// to await async code without naming the runtime. Must not be called from
/// inside a running executor task.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    shared_runtime().block_on(fut)
}

/// Spawn a fire-and-forget background task on the shared reactor. The task runs
/// to completion independently; there is no handle to await (use for best-effort
/// background work like cache refresh). orca-owned so a plugin backgrounds work
/// without naming the executor's `spawn`.
pub fn spawn_detached<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    drop(shared_runtime().spawn(fut));
}
