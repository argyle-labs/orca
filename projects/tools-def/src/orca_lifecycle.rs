//! Orca install lifecycle + admin one-shots: install / uninstall / doctor /
//! update-check / update-apply / projects-list / spec-dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Shared outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct LifecycleReport {
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DoctorEntry {
    pub category: String,
    pub status: String, // "ok" | "warn" | "error"
    pub message: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub entries: Vec<DoctorEntry>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdatePinReport {
    /// The active pin after this operation, or None if the pin was cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// True if this was an unpin operation.
    pub cleared: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectsListReport {
    pub projects: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SpecDumpReport {
    /// Orca's own OpenAPI JSON document, pretty-printed.
    pub spec: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RuntimeSpecReport {
    /// Orca version from `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// "embedded" when this binary was built with the `ui` feature on, otherwise "disabled".
    pub frontend: String,
    /// Build target triple of this binary (e.g. `aarch64-apple-darwin`).
    pub target: String,
}

// ── Args ────────────────────────────────────────────────────────────────────

macro_rules! empty_args {
    ($name:ident) => {
        #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
        #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemUpdatePinArgs {
    /// Version to pin to, e.g. "v0.0.4-rc.1". A leading `v` is optional.
    pub version: String,
}

empty_args!(SystemUpdateUnpinArgs);

// ── Tool defs ───────────────────────────────────────────────────────────────

pub struct SystemInstall;
impl OrcaToolDef for SystemInstall {
    const NAME: &'static str = "system.install";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Install orca: wire symlinks, register MCP server, install binary.";
    type Args = SystemInstallArgs;
    type Output = LifecycleReport;
}

pub struct SystemUninstall;
impl OrcaToolDef for SystemUninstall {
    const NAME: &'static str = "system.uninstall";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove binary, MCP registration, and CLAUDE.md symlinks.";
    type Args = SystemUninstallArgs;
    type Output = LifecycleReport;
}

pub struct SystemDoctor;
impl OrcaToolDef for SystemDoctor {
    const NAME: &'static str = "system.doctor";
    const DESCRIPTION: &'static str = "Validate agent files, symlinks, config, tool availability — returns ok/warn/error entries.";
    type Args = SystemDoctorArgs;
    type Output = DoctorReport;
}

pub struct SystemUpdateCheck;
impl OrcaToolDef for SystemUpdateCheck {
    const NAME: &'static str = "system.update-check";
    const DESCRIPTION: &'static str =
        "Probe GitHub releases for a newer version on `channel`. Does not apply anything.";
    type Args = SystemUpdateArgs;
    type Output = UpdateCheckReport;
}

pub struct SystemUpdateApply;
impl OrcaToolDef for SystemUpdateApply {
    const NAME: &'static str = "system.update-apply";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Download + install the latest binary on `channel`. No-op if up to date.";
    type Args = SystemUpdateArgs;
    type Output = LifecycleReport;
}

pub struct ProjectsList;
impl OrcaToolDef for ProjectsList {
    const NAME: &'static str = "projects.list";
    const DESCRIPTION: &'static str =
        "List projects (memory directories under the orca vault root).";
    type Args = ProjectsListArgs;
    type Output = ProjectsListReport;
}

pub struct SystemRuntimeSpec;
impl OrcaToolDef for SystemRuntimeSpec {
    const NAME: &'static str = "system.runtime-spec";
    const DESCRIPTION: &'static str = "Report this binary's runtime composition: whether the web UI is embedded, build target triple. \
         Used by installers to decide whether to fetch a JS runtime alongside the binary.";
    type Args = SystemRuntimeSpecArgs;
    type Output = RuntimeSpecReport;
}

pub struct SpecDump;
impl OrcaToolDef for SpecDump {
    const NAME: &'static str = "spec.dump";
    const DESCRIPTION: &'static str = "Dump orca's own OpenAPI JSON document. Used by build pipelines that don't \
         want to spin up the HTTP server.";
    type Args = SpecDumpArgs;
    type Output = SpecDumpReport;
}

pub struct SystemUpdatePin;
impl OrcaToolDef for SystemUpdatePin {
    const NAME: &'static str = "system.update-pin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Pin orca to a specific version. Future `orca update` runs will not upgrade past this version.";
    type Args = SystemUpdatePinArgs;
    type Output = UpdatePinReport;
}

pub struct SystemUpdateUnpin;
impl OrcaToolDef for SystemUpdateUnpin {
    const NAME: &'static str = "system.update-unpin";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Clear the version pin. `orca update` will resume upgrading to the latest on the configured channel.";
    type Args = SystemUpdateUnpinArgs;
    type Output = UpdatePinReport;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::lifecycle::LifecycleService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn LifecycleService>> {
        ctx.service::<Arc<dyn LifecycleService>>()
    }

    #[async_trait]
    impl OrcaTool for SystemInstall {
        async fn run(_a: SystemInstallArgs, ctx: &ToolCtx) -> Result<LifecycleReport> {
            svc(ctx)?.install().await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemUninstall {
        async fn run(_a: SystemUninstallArgs, ctx: &ToolCtx) -> Result<LifecycleReport> {
            svc(ctx)?.uninstall().await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemDoctor {
        async fn run(_a: SystemDoctorArgs, ctx: &ToolCtx) -> Result<DoctorReport> {
            svc(ctx)?.doctor().await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemUpdateCheck {
        async fn run(a: SystemUpdateArgs, ctx: &ToolCtx) -> Result<UpdateCheckReport> {
            svc(ctx)?.update_check(&a.channel).await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemUpdateApply {
        async fn run(a: SystemUpdateArgs, ctx: &ToolCtx) -> Result<LifecycleReport> {
            svc(ctx)?.update_apply(&a.channel).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProjectsList {
        async fn run(_a: ProjectsListArgs, ctx: &ToolCtx) -> Result<ProjectsListReport> {
            svc(ctx)?.projects_list().await
        }
    }
    #[async_trait]
    impl OrcaTool for SpecDump {
        async fn run(_a: SpecDumpArgs, ctx: &ToolCtx) -> Result<SpecDumpReport> {
            svc(ctx)?.spec_dump().await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemRuntimeSpec {
        async fn run(_a: SystemRuntimeSpecArgs, ctx: &ToolCtx) -> Result<RuntimeSpecReport> {
            svc(ctx)?.runtime_spec().await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemUpdatePin {
        async fn run(a: SystemUpdatePinArgs, ctx: &ToolCtx) -> Result<UpdatePinReport> {
            svc(ctx)?.update_pin(&a.version).await
        }
    }
    #[async_trait]
    impl OrcaTool for SystemUpdateUnpin {
        async fn run(_a: SystemUpdateUnpinArgs, ctx: &ToolCtx) -> Result<UpdatePinReport> {
            svc(ctx)?.update_unpin().await
        }
    }
}
