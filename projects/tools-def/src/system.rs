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

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SystemActionResult {
    pub ok: bool,
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemStatusArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemActionArgs {
    /// `install` or `uninstall`.
    pub action: String,
}

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::system::SystemService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::system::SystemService>>()
}

/// Snapshot of orca's installation: binary, ~/.claude/CLAUDE.md, vault dir, agents symlink, PKI init, MCP registration.
#[orca_tool(domain = "system", verb = "status", remote_ok = true)]
async fn system_status(
    _args: SystemStatusArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemStatusReport> {
    svc(ctx)?.status().await
}

/// [MUTATES STATE] Run orca's install or uninstall flow. Returns the per-step report.
#[orca_tool(domain = "system", verb = "action")]
async fn system_action(
    args: SystemActionArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemActionResult> {
    svc(ctx)?.action(&args.action).await
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::system::SystemService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct StubSystem {
        action_calls: Arc<AtomicUsize>,
        last_action: Arc<std::sync::Mutex<String>>,
    }
    #[async_trait]
    impl SystemService for StubSystem {
        async fn status(&self) -> Result<SystemStatusReport> {
            Ok(ok_report())
        }
        async fn action(&self, action: &str) -> Result<SystemActionResult> {
            self.action_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_action.lock().unwrap() = action.to_string();
            Ok(SystemActionResult {
                ok: action == "install",
                done: vec![action.to_string()],
                skipped: vec![],
                errors: vec![],
            })
        }
    }

    fn ctx_with_stub() -> (
        orca_utils::tool::ToolCtx,
        Arc<AtomicUsize>,
        Arc<std::sync::Mutex<String>>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(std::sync::Mutex::new(String::new()));
        let stub: Arc<dyn SystemService> = Arc::new(StubSystem {
            action_calls: calls.clone(),
            last_action: last.clone(),
        });
        let mut ctx = empty_ctx();
        ctx.register_service(stub);
        (ctx, calls, last)
    }

    #[tokio::test]
    async fn system_status_returns_service_report() {
        let (ctx, _, _) = ctx_with_stub();
        let out = system_status(SystemStatusArgs {}, &ctx).await.unwrap();
        assert!(out.binary.installed);
        assert!(out.mcp.registered);
    }

    #[tokio::test]
    async fn system_status_errors_when_service_not_registered() {
        let ctx = empty_ctx();
        let err = system_status(SystemStatusArgs {}, &ctx).await.err();
        assert!(err.is_some(), "expected error when SystemService missing");
    }

    #[tokio::test]
    async fn system_action_forwards_install_to_service() {
        let (ctx, calls, last) = ctx_with_stub();
        let out = system_action(
            SystemActionArgs {
                action: "install".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(out.ok);
        assert_eq!(out.done, vec!["install".to_string()]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*last.lock().unwrap(), "install");
    }

    #[tokio::test]
    async fn system_action_forwards_uninstall_and_reports_not_ok() {
        let (ctx, _, last) = ctx_with_stub();
        let out = system_action(
            SystemActionArgs {
                action: "uninstall".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(!out.ok);
        assert_eq!(*last.lock().unwrap(), "uninstall");
    }
}
