//! Service trait for the `system` domain — orca's own install/uninstall
//! lifecycle and the status snapshot the web UI polls.

use anyhow::Result;
use async_trait::async_trait;

use crate::system::{SystemActionResult, SystemStatusReport};

#[async_trait]
pub trait SystemService: Send + Sync {
    /// Snapshot of orca's installation state (binary, CLAUDE.md, vault, agents,
    /// PKI, MCP registration).
    async fn status(&self) -> Result<SystemStatusReport>;

    /// Run `install` or `uninstall`. Returns the per-step done/skipped/errors
    /// report.
    async fn action(&self, action: &str) -> Result<SystemActionResult>;
}
