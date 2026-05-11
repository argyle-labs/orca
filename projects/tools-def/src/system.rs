//! System domain tools — install/uninstall lifecycle + install-status snapshot.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Shared shapes ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInstalled {
    pub installed: bool,
    pub path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathLinked {
    pub linked: bool,
    pub path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathExists {
    pub exists: bool,
    pub path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInitialized {
    pub initialized: bool,
    pub path: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct McpRegistration {
    pub registered: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub struct SystemStatusReport {
    pub binary: PathInstalled,
    pub claude_md: PathLinked,
    pub vault: PathExists,
    pub agents: PathLinked,
    pub pki: PathInitialized,
    pub mcp: McpRegistration,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SystemActionResult {
    pub ok: bool,
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

// system_status
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemStatusArgs {}

pub struct SystemStatus;
impl OrcaToolDef for SystemStatus {
    const NAME: &'static str = "system_status";
    const DESCRIPTION: &'static str = "Snapshot of orca's installation: binary, ~/.claude/CLAUDE.md, vault dir, agents \
         symlink, PKI init, MCP registration.";
    type Args = SystemStatusArgs;
    type Output = SystemStatusReport;
}

// system_action
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemActionArgs {
    /// `install` or `uninstall`.
    pub action: String,
}

pub struct SystemAction;
impl OrcaToolDef for SystemAction {
    const NAME: &'static str = "system_action";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Run orca's install or uninstall flow. Returns the per-step report.";
    type Args = SystemActionArgs;
    type Output = SystemActionResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Native run impls
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::system::SystemService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn SystemService>> {
        ctx.service::<Arc<dyn SystemService>>()
    }

    #[async_trait]
    impl OrcaTool for SystemStatus {
        async fn run(_args: SystemStatusArgs, ctx: &ToolCtx) -> Result<SystemStatusReport> {
            svc(ctx)?.status().await
        }
    }

    #[async_trait]
    impl OrcaTool for SystemAction {
        async fn run(args: SystemActionArgs, ctx: &ToolCtx) -> Result<SystemActionResult> {
            svc(ctx)?.action(&args.action).await
        }
    }
}
