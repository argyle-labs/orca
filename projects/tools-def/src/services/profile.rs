//! `ProfileService` — user-scoped profile registry (the v1 single-user impl
//! always operates on `LOCAL_USER`; a future multi-tenant build can swap
//! the impl without touching the surface).

use anyhow::Result;
use async_trait::async_trait;

use crate::orca_profile::{
    ProfileCurrentReport, ProfileDetail, ProfileListReport, ProfileMutationResult,
    ProfileSharesReport,
};

#[async_trait]
pub trait ProfileService: Send + Sync {
    async fn list(&self) -> Result<ProfileListReport>;
    async fn show(&self, spec: Option<&str>) -> Result<ProfileDetail>;
    async fn current(&self) -> Result<ProfileCurrentReport>;
    async fn create(&self, name: &str, description: Option<&str>) -> Result<ProfileDetail>;
    async fn delete(&self, spec: &str) -> Result<ProfileMutationResult>;
    async fn use_profile(&self, spec: &str) -> Result<ProfileMutationResult>;
    async fn share(&self, spec: &str, user: &str, role: &str) -> Result<ProfileMutationResult>;
    async fn unshare(&self, spec: &str, user: &str) -> Result<ProfileMutationResult>;
    async fn shares(&self, spec: &str) -> Result<ProfileSharesReport>;
}
