//! Orca install lifecycle + admin one-shots: install / uninstall / doctor /
//! update-check / update-apply / projects-list / spec-dump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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
#[orca_tool(domain = "system", verb = "update-check")]
async fn system_update_check(
    args: SystemUpdateArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UpdateCheckReport> {
    svc(ctx)?.update_check(&args.channel).await
}

/// [MUTATES STATE] Download + install the latest binary on `channel`. No-op if up to date.
#[orca_tool(domain = "system", verb = "update-apply")]
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
#[orca_tool(domain = "spec", verb = "dump")]
async fn spec_dump(
    _args: SpecDumpArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SpecDumpReport> {
    svc(ctx)?.spec_dump().await
}

/// Report this binary's runtime composition: whether the web UI is embedded, build target triple. Used by installers to decide whether to fetch a JS runtime alongside the binary.
#[orca_tool(domain = "system", verb = "runtime-spec")]
async fn system_runtime_spec(
    _args: SystemRuntimeSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RuntimeSpecReport> {
    svc(ctx)?.runtime_spec().await
}
