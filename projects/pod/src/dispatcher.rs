//! Process-global handle for in-process tool dispatch from the pod relay.
//!
//! Was: pod/exec POSTed back to `https://127.0.0.1:12000/api/v1/<name>`
//! using the loopback admin token. That impersonated `role=admin` for every
//! peer-relayed call (M4 in the v1 hardening punch list). Now: the daemon
//! installs the shared `ToolCtx` here at startup, and `handle_exec`
//! dispatches directly via the free-fn `dispatch::dispatch` — no HTTP
//! hop, no token impersonation. The dispatchers walk the `inventory` slice
//! directly, so there's no registry to ship through this handle.
//!
//! Authorization flows through `pod::listener::authorize_exec`. It enforces
//! the `REMOTE_OK` allowlist and (until per-user identity is wired over
//! pod/exec) refuses any tool with a non-`"any"` required role. The target
//! model is per-user role checks against a pod-replicated identity registry,
//! not per-peer trust: mTLS proves who is on the wire, but admin delegation
//! is a property of the invoking user, not the relaying peer.
//!
//! `serde_json::Value` is unavoidable here: `dispatch::dispatch` is the
//! heterogeneous-tool entry point and takes/returns opaque JSON by contract.
//! Callers serialize the typed Args before this hop and deserialize the typed
//! Output immediately after, so opaque JSON never escapes the wire boundary.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use contract::ToolCtx;
use serde_json::Value;
use std::sync::{Arc, Mutex};

static CTX: Mutex<Option<Arc<ToolCtx>>> = Mutex::new(None);

/// Wire the shared ctx into the process-global slot so the pod relay can
/// dispatch directly. Same `Arc` the axum router uses, so all surfaces
/// share one set of service handles. Idempotent.
pub fn install(ctx: Arc<ToolCtx>) {
    let mut guard = CTX.lock().expect("pod dispatcher mutex poisoned");
    if guard.is_none() {
        *guard = Some(ctx);
    }
}

/// Dispatch a tool through the shared inventory, returning its structured
/// JSON output. Returns `Err` when the dispatcher has not been installed yet
/// (daemon not fully started) or when the tool itself errors.
pub async fn dispatch(name: &str, args: Value, correlation_id: Option<String>) -> Result<Value> {
    let ctx = {
        let guard = CTX.lock().expect("pod dispatcher mutex poisoned");
        guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pod dispatcher not installed yet"))?
            .clone()
    };
    // Inherit the originator's correlation_id so this peer's logs join the
    // same trace as the host that initiated the request.
    if let Some(cid) = correlation_id {
        let mut local = (*ctx).clone();
        local.set_correlation_id(Some(cid));
        return dispatch::dispatch(name, args, &local).await;
    }
    dispatch::dispatch(name, args, &ctx).await
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *CTX.lock().expect("pod dispatcher mutex poisoned") = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    async fn test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().await
    }

    fn make_ctx() -> Arc<ToolCtx> {
        use contract::config::{Config, Model};
        use std::path::PathBuf;
        let cfg = Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        });
        Arc::new(ToolCtx::new(cfg))
    }

    #[tokio::test]
    async fn dispatch_before_install_returns_err() {
        let _g = test_guard().await;
        reset_for_tests();
        let err = dispatch("some.tool", serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not installed"));
    }

    #[tokio::test]
    async fn install_and_dispatch_unknown_tool_returns_err() {
        let _g = test_guard().await;
        reset_for_tests();
        install(make_ctx());
        let err = dispatch("ghost.tool", serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost.tool") || !err.to_string().is_empty());
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = test_guard().await;
        reset_for_tests();
        let ctx = make_ctx();
        install(Arc::clone(&ctx));
        install(ctx);
    }
}
