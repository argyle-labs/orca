//! `LifecycleService` — orca's own install / uninstall / doctor / update /
//! projects-list / openapi-dump entry points. Catch-all for the "manage the
//! orca install" verbs that don't deserve their own service trait.

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_lifecycle::{
    DoctorReport, LifecycleReport, ProjectsListReport, RuntimeSpecReport, SpecDumpReport,
};

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
pub fn register_lifecycle(ctx: &mut orca_tool::ToolCtx, p: &impl ProvideLifecycle) {
    ctx.register_service(p.lifecycle());
}
