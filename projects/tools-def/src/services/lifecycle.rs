//! `LifecycleService` — orca's own install / uninstall / doctor / update /
//! projects-list / openapi-dump entry points. Catch-all for the "manage the
//! orca install" verbs that don't deserve their own service trait.

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_lifecycle::{
    DoctorReport, LifecycleReport, ProjectsListReport, RuntimeSpecReport, SpecDumpReport,
    SystemDevDisableOutput, SystemDevEnableOutput, SystemDevSyncOutput, UpdateCheckReport,
    UpdatePinReport,
};

#[async_trait]
pub trait LifecycleService: Send + Sync {
    async fn install(&self) -> Result<LifecycleReport>;
    async fn uninstall(&self) -> Result<LifecycleReport>;
    async fn doctor(&self) -> Result<DoctorReport>;
    async fn update_check(&self, channel: &str) -> Result<UpdateCheckReport>;
    async fn update_apply(&self, channel: &str) -> Result<LifecycleReport>;
    async fn update_pin(&self, version: &str) -> Result<UpdatePinReport>;
    async fn update_unpin(&self) -> Result<UpdatePinReport>;
    async fn projects_list(&self) -> Result<ProjectsListReport>;
    async fn spec_dump(&self) -> Result<SpecDumpReport>;
    async fn runtime_spec(&self) -> Result<RuntimeSpecReport>;
    async fn dev_enable(&self) -> Result<SystemDevEnableOutput>;
    async fn dev_disable(&self) -> Result<SystemDevDisableOutput>;
    async fn dev_sync(&self) -> Result<SystemDevSyncOutput>;
}
