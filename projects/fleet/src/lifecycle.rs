//! Orca install lifecycle + admin one-shots: install / uninstall / doctor /
//! update-check / update-apply / projects-list / spec-dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use system::system_info_types::SystemInfoReport;

#[cfg(feature = "native")]
use orca_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

// ── Shared outputs ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LifecycleReport {
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DoctorEntry {
    pub category: String,
    pub status: String, // "ok" | "warn" | "error"
    pub message: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub entries: Vec<DoctorEntry>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateCheckReport {
    pub channel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    pub up_to_date: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_url: Option<String>,
    /// Set when an update is available but blocked by a version pin.
    /// The user must run `orca update --unpin` to proceed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdatePinReport {
    /// The active pin after this operation, or None if the pin was cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// True if this was an unpin operation.
    pub cleared: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectsListReport {
    pub projects: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SpecDumpReport {
    /// Orca's own OpenAPI JSON document, pretty-printed.
    pub spec: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RuntimeSpecReport {
    /// Orca version from `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// "embedded" when this binary was built with the `ui` feature on, otherwise "disabled".
    pub frontend: String,
    /// Build target triple of this binary (e.g. `aarch64-apple-darwin`).
    pub target: String,
    /// Current daemon operating mode: "daemon" | "parked" | "dev". `None`
    /// when the state file is absent (binary not running as the registered daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Release channel marker (`stable` | `rc` | `beta` | `alpha`). `None`
    /// when no channel marker has been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Active version pin if any (`orca update --pin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// Cross-platform system snapshot (OS / hardware / process / network).
    /// `None` only if the collector failed to initialise on the peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

// ── Args ────────────────────────────────────────────────────────────────────

macro_rules! empty_args {
    ($name:ident) => {
        #[cfg_attr(feature = "cli", derive(clap::Args))]
        #[derive(Serialize, Deserialize, JsonSchema)]
        pub struct $name {}
    };
}
empty_args!(SystemDoctorArgs);
empty_args!(EmptyDeleteArgs);
empty_args!(ProjectsListArgs);
empty_args!(SpecDumpArgs);
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SystemUpdateArgs {
    /// Version or channel to switch to, then apply.
    /// Channels: "stable" | "rc" | "dev".
    /// Pinned version: "0.0.4-rc.11" (leading "v" optional).
    /// "dev" tracks GitHub HEAD via cargo-watch; others pull release binaries.
    /// Omit to apply the latest on the current channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub version: Option<String>,
    /// When set, proxy the call to the named remote peer via the pod mesh
    /// instead of running on the local host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long, hide = true))]
    pub peer_id: Option<String>,
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// [MUTATES STATE] Install orca on this host: wire symlinks, register MCP server, install binary.
#[orca_tool(domain = "system", verb = "create")]
async fn system_create(
    _args: EmptyDeleteArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    ctx.service::<Arc<dyn LifecycleService>>()?.install().await
}

/// [MUTATES STATE] Update orca on this host.
/// Optionally pass `version` to switch channel or pin before applying:
/// "stable" | "rc" | "dev" | "<semver>". "dev" tracks GitHub HEAD via
/// cargo-watch. Omit to apply the latest on the current channel.
/// When `peer_id` is set the update runs on the named peer instead of locally.
#[orca_tool(
    domain = "system",
    verb = "update",
    remote_ok = true,
    peer_dispatch = true
)]
async fn system_update(
    args: SystemUpdateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    let s = ctx.service::<Arc<dyn LifecycleService>>()?;
    if let Some(ref v) = args.version {
        s.set_version(v).await?;
    }
    s.update_apply_current().await
}

/// [MUTATES STATE] Uninstall orca from this host: remove binary, MCP registration, and CLAUDE.md symlinks.
#[orca_tool(domain = "system", verb = "delete")]
async fn system_delete(
    _args: EmptyDeleteArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    ctx.service::<Arc<dyn LifecycleService>>()?
        .uninstall()
        .await
}

/// Validate agent files, symlinks, config, tool availability — returns ok/warn/error entries.
#[orca_tool(domain = "system.diagnostic", verb = "list")]
async fn system_diagnostic_list(
    _args: SystemDoctorArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<DoctorReport> {
    ctx.service::<Arc<dyn LifecycleService>>()?.doctor().await
}

/// List projects (memory directories under the orca vault root).
#[orca_tool(domain = "namespace.project", verb = "list")]
async fn projects_list(
    _args: ProjectsListArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProjectsListReport> {
    ctx.service::<Arc<dyn LifecycleService>>()?
        .projects_list()
        .await
}

/// Dump orca's own OpenAPI JSON document. Used by build pipelines that don't want to spin up the HTTP server.
#[orca_tool(domain = "namespace.spec", verb = "detail")]
async fn spec_detail(
    _args: SpecDumpArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SpecDumpReport> {
    ctx.service::<Arc<dyn LifecycleService>>()?
        .spec_dump()
        .await
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::lifecycle::LifecycleService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubLifecycle {
        last_version: Mutex<Option<String>>,
    }

    #[async_trait]
    impl LifecycleService for StubLifecycle {
        async fn install(&self) -> Result<LifecycleReport> {
            Ok(LifecycleReport {
                done: vec!["install".into()],
                skipped: vec![],
                errors: vec![],
            })
        }
        async fn uninstall(&self) -> Result<LifecycleReport> {
            Ok(LifecycleReport {
                done: vec!["uninstall".into()],
                skipped: vec![],
                errors: vec![],
            })
        }
        async fn doctor(&self) -> Result<DoctorReport> {
            Ok(DoctorReport {
                entries: vec![DoctorEntry {
                    category: "binary".into(),
                    status: "ok".into(),
                    message: "present".into(),
                }],
            })
        }
        async fn set_version(&self, version: &str) -> Result<()> {
            *self.last_version.lock().unwrap() = Some(version.to_string());
            Ok(())
        }
        async fn update_apply_current(&self) -> Result<LifecycleReport> {
            let v = self
                .last_version
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "stable".into());
            Ok(LifecycleReport {
                done: vec![format!("update:{v}")],
                skipped: vec![],
                errors: vec![],
            })
        }
        async fn projects_list(&self) -> Result<ProjectsListReport> {
            Ok(ProjectsListReport {
                projects: vec!["alpha".into(), "beta".into()],
            })
        }
        async fn spec_dump(&self) -> Result<SpecDumpReport> {
            Ok(SpecDumpReport { spec: "{}".into() })
        }
        async fn runtime_spec(&self) -> Result<RuntimeSpecReport> {
            Ok(RuntimeSpecReport {
                version: "0.0.0".into(),
                frontend: "disabled".into(),
                target: "test".into(),
                mode: None,
                channel: None,
                pinned_to: None,
                system: None,
            })
        }
    }

    fn ctx_with_stub() -> (orca_contract::ToolCtx, Arc<StubLifecycle>) {
        let stub = Arc::new(StubLifecycle::default());
        let svc: Arc<dyn LifecycleService> = stub.clone();
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        (ctx, stub)
    }

    #[tokio::test]
    async fn system_create_forwards_to_install() {
        let (ctx, _) = ctx_with_stub();
        let r = system_create(EmptyDeleteArgs {}, &ctx).await.unwrap();
        assert_eq!(r.done, vec!["install".to_string()]);
    }

    #[tokio::test]
    async fn system_delete_forwards_to_uninstall() {
        let (ctx, _) = ctx_with_stub();
        let r = system_delete(EmptyDeleteArgs {}, &ctx).await.unwrap();
        assert_eq!(r.done, vec!["uninstall".to_string()]);
    }

    #[tokio::test]
    async fn system_update_no_version_applies_current() {
        let (ctx, _) = ctx_with_stub();
        let r = system_update(
            SystemUpdateArgs {
                version: None,
                peer_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.done, vec!["update:stable".to_string()]);
    }

    #[tokio::test]
    async fn system_update_with_version_sets_then_applies() {
        let (ctx, stub) = ctx_with_stub();
        let r = system_update(
            SystemUpdateArgs {
                version: Some("rc".into()),
                peer_id: None,
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.done, vec!["update:rc".to_string()]);
        assert_eq!(stub.last_version.lock().unwrap().as_deref(), Some("rc"));
    }

    #[tokio::test]
    async fn diagnostic_list_returns_entries_from_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_diagnostic_list(SystemDoctorArgs {}, &ctx)
            .await
            .unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].status, "ok");
    }

    // ── peer proxy tests ─────────────────────────────────────────────────────

    #[derive(Default)]
    struct StubPod {
        // serde_json::Value is intentional here: the stub captures the raw
        // args JSON so tests can assert on whatever shape the caller sent.
        #[allow(clippy::disallowed_types)]
        last_exec: Mutex<Option<(String, String, serde_json::Value)>>,
    }

    #[async_trait]
    impl crate::pod::PodService for StubPod {
        async fn list_enriched(&self) -> Result<Vec<crate::pod::PodPeerDto>> {
            Ok(vec![])
        }
        async fn accept(&self, _code: &str) -> Result<crate::pod::PodAcceptOutput> {
            anyhow::bail!("stub")
        }
        async fn trust(&self, _peer_id: &str, _on: bool) -> Result<crate::pod::PodTrustOutput> {
            anyhow::bail!("stub")
        }
        async fn push_trust(
            &self,
            _peer_id: &str,
            _on: bool,
        ) -> Result<crate::pod::PodTrustOutput> {
            anyhow::bail!("stub")
        }
        async fn ping(&self, peer_id: &str) -> crate::pod::PodPingOutput {
            crate::pod::PodPingOutput {
                ok: true,
                latency_ms: 0,
                error: None,
                peer_id: Some(peer_id.into()),
                hostname: None,
                version: None,
            }
        }
        fn discover(&self) -> Result<Vec<crate::pod::PodDiscoveryRowDto>> {
            Ok(vec![])
        }
        fn pending(&self) -> Result<Vec<crate::pod::PodPendingOfferDto>> {
            Ok(vec![])
        }
        async fn offer(
            &self,
            _addr: &str,
            _port: Option<u16>,
        ) -> Result<crate::pod::PodOfferOutput> {
            anyhow::bail!("stub")
        }
        async fn join(
            &self,
            _inviter_addr: &str,
            _port: Option<u16>,
        ) -> Result<crate::pod::PodJoinOutput> {
            anyhow::bail!("stub")
        }
        async fn leave_peer(&self, _peer_id: &str) -> Result<crate::pod::PodLeaveOutput> {
            anyhow::bail!("stub")
        }
        fn cert_status(&self) -> Result<crate::pod::PodCertStatusOutput> {
            anyhow::bail!("stub")
        }
        fn get_self_secure(&self) -> Result<bool> {
            Ok(false)
        }
        async fn set_self_secure(&self, on: bool) -> Result<bool> {
            Ok(on)
        }
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<crate::pod::PodExecDispatch> {
            *self.last_exec.lock().unwrap() = Some((peer.into(), tool.into(), args.clone()));
            let result = serde_json::json!({
                "done": [format!("update:{}", args["version"].as_str().unwrap_or(""))],
                "skipped": [],
                "errors": []
            });
            Ok(crate::pod::PodExecDispatch {
                peer: peer.into(),
                tool: tool.into(),
                result,
            })
        }
    }

    struct StubRemoteExec(Arc<StubPod>);

    #[async_trait]
    impl orca_contract::RemoteExec for StubRemoteExec {
        #[allow(clippy::disallowed_types)]
        async fn exec(
            &self,
            peer: &str,
            tool: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value> {
            let dispatch =
                <StubPod as crate::pod::PodService>::exec(&self.0, peer, tool, args).await?;
            Ok(dispatch.result)
        }
    }

    fn ctx_with_lifecycle_and_pod() -> (orca_contract::ToolCtx, Arc<StubLifecycle>, Arc<StubPod>) {
        let lifecycle = Arc::new(StubLifecycle::default());
        let pod = Arc::new(StubPod::default());
        let mut ctx = empty_ctx();
        ctx.register_service(Arc::clone(&lifecycle) as Arc<dyn LifecycleService>);
        ctx.register_service(Arc::clone(&pod) as Arc<dyn crate::pod::PodService>);
        let remote: Arc<dyn orca_contract::RemoteExec> = Arc::new(StubRemoteExec(Arc::clone(&pod)));
        ctx.register_service(remote);
        (ctx, lifecycle, pod)
    }

    #[tokio::test]
    async fn system_update_proxies_to_peer_when_peer_id_set() {
        use orca_contract::OrcaTool;
        let (ctx, _, pod) = ctx_with_lifecycle_and_pod();
        let r = SystemUpdate::run(
            SystemUpdateArgs {
                version: Some("rc".into()),
                peer_id: Some("peer.abc".into()),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.done, vec!["update:rc".to_string()]);
        let (peer, tool, args) = pod.last_exec.lock().unwrap().clone().unwrap();
        assert_eq!(peer, "peer.abc");
        assert_eq!(tool, "system.update");
        assert_eq!(args["version"], "rc");
    }

    #[tokio::test]
    async fn projects_list_returns_service_projects() {
        let (ctx, _) = ctx_with_stub();
        let r = projects_list(ProjectsListArgs {}, &ctx).await.unwrap();
        assert_eq!(r.projects, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn spec_detail_returns_service_spec() {
        let (ctx, _) = ctx_with_stub();
        let r = spec_detail(SpecDumpArgs {}, &ctx).await.unwrap();
        assert_eq!(r.spec, "{}");
    }

    #[tokio::test]
    async fn svc_errors_when_service_not_registered() {
        let ctx = empty_ctx();
        assert!(
            system_create(EmptyDeleteArgs {}, &ctx).await.is_err(),
            "expected error when LifecycleService missing"
        );
    }
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait LifecycleService: Send + Sync {
    async fn install(&self) -> Result<LifecycleReport>;
    async fn uninstall(&self) -> Result<LifecycleReport>;
    async fn doctor(&self) -> Result<DoctorReport>;
    /// Switch channel or pin: "stable" | "rc" | "dev" | "<semver>".
    async fn set_version(&self, version: &str) -> Result<()>;
    /// Apply the latest release on the current channel (or dev-sync if channel = dev).
    async fn update_apply_current(&self) -> Result<LifecycleReport>;
    async fn projects_list(&self) -> Result<ProjectsListReport>;
    async fn spec_dump(&self) -> Result<SpecDumpReport>;
    async fn runtime_spec(&self) -> Result<RuntimeSpecReport>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvideLifecycle {
    fn lifecycle(&self) -> std::sync::Arc<dyn LifecycleService>;
}

/// Register a `LifecycleService` into `ToolCtx`.
pub fn register_lifecycle(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideLifecycle) {
    ctx.register_service(p.lifecycle());
}
