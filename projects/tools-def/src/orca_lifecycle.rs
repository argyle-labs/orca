//! Orca install lifecycle + admin one-shots: install / uninstall / doctor /
//! update-check / update-apply / projects-list / spec-dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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

/// Cross-platform OS / hardware / process / network snapshot. Every field
/// is optional so the same shape works on macOS, Linux, and (eventually)
/// Windows — a collector failure leaves the field `None` rather than
/// breaking the whole report.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
pub struct SystemInfoReport {
    // ── OS ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    /// Linux distro long name (`Ubuntu 24.04.2 LTS`). `None` on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,

    // ── Virtualization ──
    /// Hypervisor / container kind: `kvm`, `qemu`, `vmware`, `lxc`,
    /// `docker`, `none`, etc. Linux-only — read from `/sys/class/dmi/id/`
    /// + `/proc/1/cgroup`. macOS reports `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization: Option<String>,
    /// DMI system vendor (`QEMU`, `Dell Inc.`, `LENOVO`, ...). Linux-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmi_vendor: Option<String>,
    /// DMI product name (`Standard PC (i440FX + PIIX, 1996)`, ...). Linux-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dmi_product: Option<String>,
    /// Proxmox role inferred from on-disk markers: `"host"` when
    /// `/etc/pve/` (pmxcfs) is mounted, `"guest"` when the inference
    /// layer matches this VM's MAC to a PVE host's tap interface,
    /// otherwise `None`. NEVER set by user config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxmox_role: Option<String>,

    // ── Hardware ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_logical: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_physical: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_available_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_total_mb: Option<u64>,

    // ── Host / uptime ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_uptime_secs: Option<u64>,
    /// Unix load averages (1/5/15 min). `None` on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_5: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_avg_15: Option<f64>,

    // ── This orca process ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_started_at_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_uptime_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_rss_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_threads: Option<u32>,

    // ── Storage (filesystem hosting ~/.orca) ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_fs_total_gb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orca_fs_avail_gb: Option<u64>,

    // ── Runtime / integrations ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker_present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_peer_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_paired_count: Option<u32>,

    // ── Network interfaces ──
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interfaces: Vec<NetIfaceDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv6: Option<String>,

    /// Wall-clock when this snapshot was collected. Cached snapshots may be
    /// up to ~30s stale; consumers use this to decide whether to trust a
    /// metric like load average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_at_unix: Option<i64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct NetIfaceDto {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ipv4: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ipv6: Vec<String>,
    /// True for loopback interfaces (lo / lo0).
    #[serde(default)]
    pub loopback: bool,
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
empty_args!(SystemInstallArgs);
empty_args!(SystemUninstallArgs);
empty_args!(SystemDoctorArgs);
empty_args!(ProjectsListArgs);
empty_args!(SpecDumpArgs);
empty_args!(SystemRuntimeSpecArgs);

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemUpdateArgs {
    /// "stable" (default) | "rc" | "beta" | "alpha".
    #[serde(default = "default_channel")]
    #[cfg_attr(feature = "cli", arg(default_value = "stable"))]
    pub channel: String,
}
fn default_channel() -> String {
    "stable".into()
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemUpdatePinArgs {
    /// Version to pin to, e.g. "v0.0.4-rc.1". A leading `v` is optional.
    pub version: String,
}

empty_args!(SystemUpdateUnpinArgs);
empty_args!(SystemDevEnableArgs);
empty_args!(SystemDevDisableArgs);
empty_args!(SystemDevSyncArgs);

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemDevEnableOutput {
    /// Path to the git checkout used as the dev source.
    pub repo_path: String,
    /// Whether the repo was freshly cloned (true) or already present (false).
    pub cloned: bool,
    /// Whether the production daemon was parked as part of this call.
    pub daemon_parked: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemDevDisableOutput {
    /// Whether the dev process was running and was killed.
    pub dev_process_stopped: bool,
    /// Whether the production daemon reclaimed the port.
    pub daemon_reclaimed: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemDevSyncOutput {
    /// Number of commits pulled.
    pub commits_pulled: u32,
    /// Whether the repo was already up to date.
    pub already_up_to_date: bool,
    /// Raw output from git pull for diagnostics.
    pub detail: String,
}

// ── Tools ───────────────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::lifecycle::LifecycleService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::lifecycle::LifecycleService>>()
}

/// [MUTATES STATE] Install orca: wire symlinks, register MCP server, install binary.
#[orca_tool(domain = "system", verb = "install")]
async fn system_install(
    _args: SystemInstallArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    svc(ctx)?.install().await
}

/// [MUTATES STATE] Remove binary, MCP registration, and CLAUDE.md symlinks.
#[orca_tool(domain = "system", verb = "uninstall")]
async fn system_uninstall(
    _args: SystemUninstallArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    svc(ctx)?.uninstall().await
}

/// Validate agent files, symlinks, config, tool availability — returns ok/warn/error entries.
#[orca_tool(domain = "system", verb = "doctor")]
async fn system_doctor(
    _args: SystemDoctorArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<DoctorReport> {
    svc(ctx)?.doctor().await
}

/// Probe GitHub releases for a newer version on `channel`. Does not apply anything.
#[orca_tool(domain = "system", verb = "update-check", remote_ok = true)]
async fn system_update_check(
    args: SystemUpdateArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UpdateCheckReport> {
    svc(ctx)?.update_check(&args.channel).await
}

/// [MUTATES STATE] Download + install the latest binary on `channel`. No-op if up to date.
#[orca_tool(domain = "system", verb = "update-apply", remote_ok = true)]
async fn system_update_apply(
    args: SystemUpdateArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<LifecycleReport> {
    svc(ctx)?.update_apply(&args.channel).await
}

/// [MUTATES STATE] Pin orca to a specific version. Future `orca update` runs will not upgrade past this version.
#[orca_tool(domain = "system", verb = "update-pin")]
async fn system_update_pin(
    args: SystemUpdatePinArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UpdatePinReport> {
    svc(ctx)?.update_pin(&args.version).await
}

/// [MUTATES STATE] Clear the version pin. `orca update` will resume upgrading to the latest on the configured channel.
#[orca_tool(domain = "system", verb = "update-unpin")]
async fn system_update_unpin(
    _args: SystemUpdateUnpinArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UpdatePinReport> {
    svc(ctx)?.update_unpin().await
}

/// List projects (memory directories under the orca vault root).
#[orca_tool(domain = "projects", verb = "list")]
async fn projects_list(
    _args: ProjectsListArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProjectsListReport> {
    svc(ctx)?.projects_list().await
}

/// Dump orca's own OpenAPI JSON document. Used by build pipelines that don't want to spin up the HTTP server.
#[orca_tool(domain = "spec", verb = "detail")]
async fn spec_detail(
    _args: SpecDumpArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SpecDumpReport> {
    svc(ctx)?.spec_dump().await
}

/// Report this binary's runtime composition: whether the web UI is embedded, build target triple. Used by installers to decide whether to fetch a JS runtime alongside the binary.
#[orca_tool(domain = "system", verb = "runtime-spec", remote_ok = true)]
async fn system_runtime_spec(
    _args: SystemRuntimeSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RuntimeSpecReport> {
    svc(ctx)?.runtime_spec().await
}

/// Clone the orca repo (if not present) and start cargo watch, parking the production daemon.
/// Idempotent — safe to call if dev mode is already active.
#[orca_tool(
    domain = "system",
    verb = "dev_enable",
    remote_ok = true,
    role = "admin"
)]
async fn system_dev_enable(
    _args: SystemDevEnableArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemDevEnableOutput> {
    svc(ctx)?.dev_enable().await
}

/// Stop cargo watch and let the production daemon reclaim the port.
#[orca_tool(domain = "system", verb = "dev_disable", role = "admin")]
async fn system_dev_disable(
    _args: SystemDevDisableArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemDevDisableOutput> {
    svc(ctx)?.dev_disable().await
}

/// git pull in the dev checkout; cargo watch detects the changes and restarts automatically.
/// No-op (returns already_up_to_date) if dev mode is not active.
#[orca_tool(domain = "system", verb = "dev_sync", remote_ok = true, role = "admin")]
async fn system_dev_sync(
    _args: SystemDevSyncArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SystemDevSyncOutput> {
    svc(ctx)?.dev_sync().await
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::lifecycle::LifecycleService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StubLifecycle {
        last_channel: Mutex<Option<String>>,
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
        async fn update_check(&self, channel: &str) -> Result<UpdateCheckReport> {
            *self.last_channel.lock().unwrap() = Some(channel.to_string());
            Ok(UpdateCheckReport {
                channel: channel.to_string(),
                latest: Some("v9.9.9".into()),
                up_to_date: false,
                asset_url: None,
                pinned_to: None,
            })
        }
        async fn update_apply(&self, channel: &str) -> Result<LifecycleReport> {
            *self.last_channel.lock().unwrap() = Some(channel.to_string());
            Ok(LifecycleReport {
                done: vec![format!("update:{channel}")],
                skipped: vec![],
                errors: vec![],
            })
        }
        async fn update_pin(&self, version: &str) -> Result<UpdatePinReport> {
            *self.last_version.lock().unwrap() = Some(version.to_string());
            Ok(UpdatePinReport {
                pinned_to: Some(version.to_string()),
                cleared: false,
            })
        }
        async fn update_unpin(&self) -> Result<UpdatePinReport> {
            Ok(UpdatePinReport {
                pinned_to: None,
                cleared: true,
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
        async fn dev_enable(&self) -> Result<SystemDevEnableOutput> {
            Ok(SystemDevEnableOutput {
                repo_path: "/tmp/orca".into(),
                cloned: true,
                daemon_parked: true,
            })
        }
        async fn dev_disable(&self) -> Result<SystemDevDisableOutput> {
            Ok(SystemDevDisableOutput {
                dev_process_stopped: true,
                daemon_reclaimed: true,
            })
        }
        async fn dev_sync(&self) -> Result<SystemDevSyncOutput> {
            Ok(SystemDevSyncOutput {
                commits_pulled: 3,
                already_up_to_date: false,
                detail: "pulled".into(),
            })
        }
    }

    fn ctx_with_stub() -> (orca_utils::tool::ToolCtx, Arc<StubLifecycle>) {
        let stub = Arc::new(StubLifecycle::default());
        let svc: Arc<dyn LifecycleService> = stub.clone();
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        (ctx, stub)
    }

    #[test]
    fn default_channel_is_stable() {
        assert_eq!(default_channel(), "stable");
    }

    #[tokio::test]
    async fn install_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_install(SystemInstallArgs {}, &ctx).await.unwrap();
        assert_eq!(r.done, vec!["install".to_string()]);
    }

    #[tokio::test]
    async fn uninstall_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_uninstall(SystemUninstallArgs {}, &ctx)
            .await
            .unwrap();
        assert_eq!(r.done, vec!["uninstall".to_string()]);
    }

    #[tokio::test]
    async fn doctor_returns_entries_from_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_doctor(SystemDoctorArgs {}, &ctx).await.unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.entries[0].status, "ok");
    }

    #[tokio::test]
    async fn update_check_forwards_channel() {
        let (ctx, stub) = ctx_with_stub();
        let r = system_update_check(
            SystemUpdateArgs {
                channel: "rc".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.channel, "rc");
        assert_eq!(stub.last_channel.lock().unwrap().as_deref(), Some("rc"));
    }

    #[tokio::test]
    async fn update_apply_forwards_channel() {
        let (ctx, stub) = ctx_with_stub();
        let r = system_update_apply(
            SystemUpdateArgs {
                channel: "beta".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.done, vec!["update:beta".to_string()]);
        assert_eq!(stub.last_channel.lock().unwrap().as_deref(), Some("beta"));
    }

    #[tokio::test]
    async fn update_pin_forwards_version() {
        let (ctx, stub) = ctx_with_stub();
        let r = system_update_pin(
            SystemUpdatePinArgs {
                version: "v1.2.3".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(r.pinned_to.as_deref(), Some("v1.2.3"));
        assert!(!r.cleared);
        assert_eq!(stub.last_version.lock().unwrap().as_deref(), Some("v1.2.3"));
    }

    #[tokio::test]
    async fn update_unpin_clears() {
        let (ctx, _) = ctx_with_stub();
        let r = system_update_unpin(SystemUpdateUnpinArgs {}, &ctx)
            .await
            .unwrap();
        assert!(r.cleared);
        assert!(r.pinned_to.is_none());
    }

    #[tokio::test]
    async fn projects_list_returns_service_projects() {
        let (ctx, _) = ctx_with_stub();
        let r = projects_list(ProjectsListArgs {}, &ctx).await.unwrap();
        assert_eq!(r.projects, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn spec_dump_returns_service_spec() {
        let (ctx, _) = ctx_with_stub();
        let r = spec_detail(SpecDumpArgs {}, &ctx).await.unwrap();
        assert_eq!(r.spec, "{}");
    }

    #[tokio::test]
    async fn runtime_spec_returns_service_report() {
        let (ctx, _) = ctx_with_stub();
        let r = system_runtime_spec(SystemRuntimeSpecArgs {}, &ctx)
            .await
            .unwrap();
        assert_eq!(r.frontend, "disabled");
    }

    #[tokio::test]
    async fn dev_enable_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_dev_enable(SystemDevEnableArgs {}, &ctx)
            .await
            .unwrap();
        assert!(r.cloned);
        assert!(r.daemon_parked);
    }

    #[tokio::test]
    async fn dev_disable_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_dev_disable(SystemDevDisableArgs {}, &ctx)
            .await
            .unwrap();
        assert!(r.dev_process_stopped);
        assert!(r.daemon_reclaimed);
    }

    #[tokio::test]
    async fn dev_sync_forwards_to_service() {
        let (ctx, _) = ctx_with_stub();
        let r = system_dev_sync(SystemDevSyncArgs {}, &ctx).await.unwrap();
        assert_eq!(r.commits_pulled, 3);
    }

    #[tokio::test]
    async fn svc_errors_when_service_not_registered() {
        let ctx = empty_ctx();
        let err = system_install(SystemInstallArgs {}, &ctx).await.err();
        assert!(
            err.is_some(),
            "expected error when LifecycleService missing"
        );
    }
}
