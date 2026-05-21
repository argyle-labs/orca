//! System domain tools — install/uninstall lifecycle + install-status snapshot.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Shared shapes ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInstalled {
    pub installed: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathLinked {
    pub linked: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathExists {
    pub exists: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInitialized {
    pub initialized: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct McpRegistration {
    pub registered: bool,
}

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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemStatusArgs {}

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::system::SystemService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::system::SystemService>>()
}

/// Snapshot of orca's installation: binary, ~/.claude/CLAUDE.md, vault dir, agents symlink, PKI init, MCP registration.
#[orca_tool(domain = "system", verb = "detail", remote_ok = true)]
async fn system_detail(
    _args: SystemStatusArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemStatusReport> {
    svc(ctx)?.status().await
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::system::SystemService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;

    fn ok_report() -> SystemStatusReport {
        SystemStatusReport {
            binary: PathInstalled {
                installed: true,
                path: "/usr/local/bin/orca".into(),
            },
            claude_md: PathLinked {
                linked: true,
                path: "~/.claude/CLAUDE.md".into(),
            },
            vault: PathExists {
                exists: true,
                path: "~/.orca".into(),
            },
            agents: PathLinked {
                linked: false,
                path: "~/.claude/agents".into(),
            },
            pki: PathInitialized {
                initialized: true,
                path: "~/.orca/pki".into(),
            },
            mcp: McpRegistration { registered: true },
        }
    }

    struct StubSystem;
    #[async_trait]
    impl SystemService for StubSystem {
        async fn status(&self) -> Result<SystemStatusReport> {
            Ok(ok_report())
        }
    }

    fn ctx_with_stub() -> orca_utils::tool::ToolCtx {
        let stub: Arc<dyn SystemService> = Arc::new(StubSystem);
        let mut ctx = empty_ctx();
        ctx.register_service(stub);
        ctx
    }

    #[tokio::test]
    async fn system_detail_returns_service_report() {
        let ctx = ctx_with_stub();
        let out = system_detail(SystemStatusArgs {}, &ctx).await.unwrap();
        assert!(out.binary.installed);
        assert!(out.mcp.registered);
    }

    #[tokio::test]
    async fn system_detail_errors_when_service_not_registered() {
        let ctx = empty_ctx();
        let err = system_detail(SystemStatusArgs {}, &ctx).await.err();
        assert!(err.is_some(), "expected error when SystemService missing");
    }
}
