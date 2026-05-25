//! Service trait for the `system` domain — orca's own install/uninstall
//! lifecycle and the status snapshot the web UI polls.

use anyhow::Result;
use async_trait::async_trait;

use crate::system::SystemStatusReport;

#[async_trait]
pub trait SystemService: Send + Sync {
    /// Snapshot of orca's installation state (binary, CLAUDE.md, vault, agents,
    /// PKI, MCP registration).
    async fn status(&self) -> Result<SystemStatusReport>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvideSystem {
    fn system(&self) -> std::sync::Arc<dyn SystemService>;
}

/// Register a `SystemService` into `ToolCtx`.
pub fn register_system(ctx: &mut orca_tool::ToolCtx, p: &impl ProvideSystem) {
    ctx.register_service(p.system());
}
