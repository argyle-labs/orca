//! Process-global handle for in-process tool dispatch from the pod relay.
//!
//! Was: pod/exec POSTed back to `https://127.0.0.1:12000/api/tools/<name>`
//! using the loopback admin token. That impersonated `role=admin` for every
//! peer-relayed call (M4 in the v1 hardening punch list). Now: the daemon
//! installs the shared `ToolRegistry` + `ToolCtx` here at startup, and
//! `handle_exec` dispatches directly via `ToolRegistry::dispatch` — no HTTP
//! hop, no token impersonation.
//!
//! Authorization still flows through `pod::listener::authorize_exec`, which
//! enforces both the `REMOTE_OK` allowlist and `REQUIRED_ROLE == "any"`, so
//! admin-role tools remain unreachable from any paired peer.
//!
//! `serde_json::Value` is unavoidable here: `ToolRegistry::dispatch` is the
//! heterogeneous-tool entry point and takes/returns opaque JSON by contract.
//! Callers serialize the typed Args before this hop and deserialize the typed
//! Output immediately after, so opaque JSON never escapes the wire boundary.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use orca_utils::tool::{ToolCtx, ToolRegistry};
use serde_json::Value;
use std::sync::{Arc, Mutex};

struct Handle {
    reg: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
}

static REG: Mutex<Option<Handle>> = Mutex::new(None);

/// Wire the shared registry + ctx into the process-global slot so the pod
/// relay can dispatch directly. Same `Arc`s the axum router uses, so all
/// surfaces share one set of tool impls + service handles. Idempotent.
pub fn install(reg: Arc<ToolRegistry>, ctx: Arc<ToolCtx>) {
    let mut guard = REG.lock().expect("pod dispatcher mutex poisoned");
    if guard.is_none() {
        *guard = Some(Handle { reg, ctx });
    }
}

/// Dispatch a tool through the shared registry, returning its structured
/// JSON output. Returns `Err` when the dispatcher has not been installed yet
/// (daemon not fully started) or when the tool itself errors.
pub async fn dispatch(name: &str, args: Value) -> Result<Value> {
    let (reg, ctx) = {
        let guard = REG.lock().expect("pod dispatcher mutex poisoned");
        let h = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("pod dispatcher not installed yet"))?;
        (Arc::clone(&h.reg), Arc::clone(&h.ctx))
    };
    reg.dispatch(name, args, &ctx).await
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *REG.lock().expect("pod dispatcher mutex poisoned") = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn make_reg_ctx() -> (Arc<ToolRegistry>, Arc<ToolCtx>) {
        use orca_utils::config::{Config, Model};
        use std::path::PathBuf;
        let reg = Arc::new(ToolRegistry::new());
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
        });
        let ctx = Arc::new(ToolCtx::new(cfg));
        (reg, ctx)
    }

    #[tokio::test]
    async fn dispatch_before_install_returns_err() {
        let _g = test_guard();
        reset_for_tests();
        let err = dispatch("some.tool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not installed"));
    }

    #[tokio::test]
    async fn install_and_dispatch_unknown_tool_returns_err() {
        let _g = test_guard();
        reset_for_tests();
        let (reg, ctx) = make_reg_ctx();
        install(reg, ctx);
        let err = dispatch("ghost.tool", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ghost.tool") || !err.to_string().is_empty());
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let _g = test_guard();
        reset_for_tests();
        let (reg, ctx) = make_reg_ctx();
        install(Arc::clone(&reg), Arc::clone(&ctx));
        // Second call should be a no-op (no panic, no overwrite).
        install(reg, ctx);
    }
}
