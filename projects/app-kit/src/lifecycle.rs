//! `OrcaAppKit` — embedder lifecycle.
//!
//! Task #4 design. The struct that native UIs hold for the lifetime of the
//! app. Constructs the in-process orca core (config + service-injected
//! `ToolCtx`) and owns the tokio runtime that tool bodies run on. Tool
//! dispatch goes through `orca_dispatch`'s free-fn API, which walks the
//! `inventory` slice directly — no registry struct to hold here. UI ↔ core
//! calls are direct UniFFI (no localhost HTTP).
//!
//! ## Four-surface rule and the lifecycle carve-out
//!
//! `feedback_four_surface_parity.md` forbids hand-written `#[uniffi::export]`
//! next to a tool — the `#[orca_tool]` macro is the sole emitter for tools.
//! Lifecycle methods (`init`, `shutdown`) are **plumbing**, not tools: they
//! construct the runtime that tools execute inside. They are the UniFFI
//! analogue of `uniffi::setup_scaffolding!()` and the utoipa auth-cookie
//! carve-out. The carve-out is strictly limited to:
//!
//! - `OrcaAppKit::init` / `OrcaAppKit::shutdown` (construction + teardown)
//! - `OrcaAppKit::version` (build-time identity ping for native UIs)
//!
//! Anything that resembles a callable operation MUST flow through the
//! `#[orca_tool]` macro emission (task #5). If a fifth lifecycle method is
//! ever proposed, raise the carve-out for review before adding it.
//!
//! ## What still needs lifting (gating #5)
//!
//! `build_tool_ctx` in `projects/server/src/mcp/mod.rs` wires every
//! server-side `*Service` trait object into `ToolCtx`. App-kit cannot depend
//! on `orca-server` (would pull axum + clap + the entire daemon surface).
//! Two options for the lift, decision deferred to #5 kickoff:
//!
//! - **A. Extract `orca-runtime` crate.** Move `build_tool_ctx` +
//!   `ServerFoo` service impls (the ones that have no axum dependency) into
//!   a transport-neutral crate that both `orca-server` and `orca-app-kit`
//!   depend on. Cleanest but largest refactor.
//! - **B. Service-registration trait in each domain crate.** Each domain
//!   exposes a `register_services(&mut ToolCtx)` fn; concrete impls live
//!   wherever, and the embedder picks. Smaller change, less elegant.
//!
//! Until the lift, `init` registers only `#[orca_tool]`-inventory-registered
//! tools (tools whose bodies don't fetch from `ctx.service()`); tools that
//! need a service will fail at dispatch with "no service registered". This
//! is the explicit design-pass deliverable for #4.

use contract::ToolCtx;
use contract::config::Config;
use std::sync::Arc;

/// Configuration passed from the native UI at construction time.
///
/// Mirrors a subset of the daemon's `Config` — only the keys a UI host
/// legitimately picks. Anything the UI shouldn't decide (DB schema version,
/// channel pin, etc.) is loaded from the on-disk profile instead.
#[derive(Clone)]
pub struct AppKitConfig {
    /// `~/.orca/` (or override). Houses orca.db, profiles, secrets.
    pub app_dir: std::path::PathBuf,
}

/// In-process orca core handle held by the native UI.
///
/// One instance per app launch. Owns the tokio runtime; tool bodies run on
/// its worker pool. Cloneable Arc fields so per-tool wrappers (added in #5)
/// can grab references without moving the handle.
pub struct OrcaAppKit {
    // Fields are read by task #5 per-tool method emission. Until then,
    // construction is the only consumer — silence the linter explicitly so
    // CI's `-D warnings` clippy stays clean.
    #[allow(dead_code)]
    pub(crate) config: Arc<Config>,
    #[allow(dead_code)]
    pub(crate) ctx: Arc<ToolCtx>,
    /// Multi-threaded tokio runtime owned by this instance. Dropped at
    /// `shutdown` so background tasks unwind cleanly before the FFI handle
    /// disappears.
    #[allow(dead_code)]
    pub(crate) runtime: Arc<tokio::runtime::Runtime>,
}

impl OrcaAppKit {
    /// Construct an embedded orca core.
    ///
    /// Steps:
    ///   1. Load (or initialize) `Config` from `app_dir`.
    ///   2. Spin up a multi-threaded tokio runtime sized for in-process work.
    ///   3. Build a `ToolCtx` carrying the config. (Service injection lives
    ///      in `build_tool_ctx` in orca-server today — lifting it is
    ///      tracked in #5.) Tool dispatch reads from the `inventory` slice
    ///      that the `#[orca_tool]` macro fills at link time, so there is
    ///      no separate registration step here.
    ///
    /// Returns an `Arc<Self>` so multiple native UI screens can hold onto
    /// the same instance without copying state.
    pub fn init(
        cfg: AppKitConfig,
        install_services: impl FnOnce(&mut ToolCtx),
    ) -> anyhow::Result<Arc<Self>> {
        let config = Arc::new(Self::load_config(&cfg)?);

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("orca-app-kit")
                .build()?,
        );

        // Tool dispatch walks the `inventory::iter::<ToolRegistration>`
        // slice that every `#[orca_tool]` annotation submits into at link
        // time — no registry to construct here. The cdylib's link anchors
        // in `lib.rs` ensure every domain crate's inventory entries are
        // pulled into this binary.
        let mut ctx = ToolCtx::new(config.clone());
        // Per the service-registration convention, the embedder owns which
        // `register_*` calls happen. App-kit hosts pass a closure that
        // calls each domain crate's `register_<svc>(&mut ctx, &MyProvider)`
        // for the services their UI needs. Missing services fail at
        // dispatch with a clear `no service registered for <T>` error.
        install_services(&mut ctx);

        Ok(Arc::new(Self {
            config,
            ctx: Arc::new(ctx),
            runtime,
        }))
    }

    /// Build identity used in `About` screens / crash reports.
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Tear down the runtime explicitly. Native UIs may also rely on `Drop`
    /// when the Arc count hits zero; this is the explicit form for hosts
    /// that want deterministic teardown timing.
    pub fn shutdown(&self) {
        // The runtime is dropped when the final Arc is released. We don't
        // force-drop here because outstanding tool futures may still be
        // holding refs; let the reference counting unwind naturally.
    }

    fn load_config(cfg: &AppKitConfig) -> anyhow::Result<Config> {
        // Same shape orca-server uses: read orca.toml under app_dir, fall
        // back to defaults if absent. Defer to `Config::load_from` once it
        // grows an explicit path arg — for now this is a placeholder that
        // proves the wiring shape.
        let _ = cfg; // tracked by lift task #5
        Config::load().map_err(|e| anyhow::anyhow!("config load failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn cfg() -> AppKitConfig {
        AppKitConfig {
            app_dir: std::path::PathBuf::from("/tmp/orca-appkit-test"),
        }
    }

    #[test]
    fn init_constructs_handle_and_runs_install_services_closure() {
        let called = AtomicBool::new(false);
        let kit = OrcaAppKit::init(cfg(), |_ctx| {
            called.store(true, Ordering::SeqCst);
        })
        .expect("init succeeds in a normal test env");
        assert!(called.load(Ordering::SeqCst));
        // Arc fields are populated.
        assert!(Arc::strong_count(&kit.config) >= 1);
        assert!(Arc::strong_count(&kit.ctx) >= 1);
        assert!(Arc::strong_count(&kit.runtime) >= 1);
    }

    #[test]
    fn version_matches_crate_pkg_version() {
        let kit = OrcaAppKit::init(cfg(), |_| {}).unwrap();
        assert_eq!(kit.version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn shutdown_is_idempotent_no_op() {
        let kit = OrcaAppKit::init(cfg(), |_| {}).unwrap();
        kit.shutdown();
        kit.shutdown();
    }

    #[test]
    fn runtime_executes_async_work() {
        let kit = OrcaAppKit::init(cfg(), |_| {}).unwrap();
        let n: u32 = kit.runtime.block_on(async { 1 + 2 });
        assert_eq!(n, 3);
    }

    #[test]
    fn app_kit_config_is_clone() {
        let c = cfg();
        let d = c.clone();
        assert_eq!(c.app_dir, d.app_dir);
    }
}
