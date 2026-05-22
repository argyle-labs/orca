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

use anyhow::Result;
use orca_utils::tool::{ToolCtx, ToolRegistry};
use serde_json::Value;
use std::sync::{Arc, OnceLock};

struct Handle {
    reg: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
}

/// Installed once by the daemon after the shared registry is built. First
/// call wins; subsequent calls are no-ops.
static REG: OnceLock<Handle> = OnceLock::new();

/// Wire the shared registry + ctx into the process-global slot so the pod
/// relay can dispatch directly. Same `Arc`s the axum router uses, so all
/// surfaces share one set of tool impls + service handles. Idempotent.
pub fn install(reg: Arc<ToolRegistry>, ctx: Arc<ToolCtx>) {
    let _ = REG.set(Handle { reg, ctx });
}

/// Dispatch a tool through the shared registry, returning its structured
/// JSON output. Returns `Err` when the dispatcher has not been installed yet
/// (daemon not fully started) or when the tool itself errors.
pub async fn dispatch(name: &str, args: Value) -> Result<Value> {
    let handle = REG
        .get()
        .ok_or_else(|| anyhow::anyhow!("pod dispatcher not installed yet"))?;
    handle.reg.dispatch(name, args, &handle.ctx).await
}
