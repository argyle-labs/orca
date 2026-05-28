//! Profile registry — list/show/current/create/delete/use/share/unshare/shares.
//!
//! v1 single-user always operates on `LOCAL_USER`; multi-tenant arrives by
//! swapping the `ProfileService` impl, not by changing the surface.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use orca_macro::orca_tool;
use std::sync::Arc;

// ── Shared rows ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub is_active: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ProfileDetail {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub root: String,
    /// `owner` | `collaborator` | `viewer`
    pub access: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileListReport {
    pub profiles: Vec<ProfileSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCurrentReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ProfileSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileMutationResult {
    pub id: String,
    pub name: String,
    pub changed: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShareEntry {
    pub user_id: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileSharesReport {
    pub profile_id: String,
    pub shares: Vec<ProfileShareEntry>,
}

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileListArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShowArgs {
    /// Profile id or name. Omit to show the active profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCurrentArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCreateArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileSpecArgs {
    /// Profile id or name.
    pub spec: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShareArgs {
    pub spec: String,
    pub user: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileUnshareArgs {
    pub spec: String,
    pub user: String,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

/// List all profiles the current user can access (owned + shared).
#[orca_tool(domain = "namespace", verb = "list")]
async fn profile_list(
    _args: ProfileListArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileListReport> {
    ctx.service::<Arc<dyn ProfileService>>()?.list().await
}

/// Show details of a profile (defaults to the active one).
#[orca_tool(domain = "namespace", verb = "show")]
async fn profile_show(
    args: ProfileShowArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileDetail> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .show(args.spec.as_deref())
        .await
}

/// Show the currently active profile (or None).
#[orca_tool(domain = "namespace", verb = "current")]
async fn profile_current(
    _args: ProfileCurrentArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileCurrentReport> {
    ctx.service::<Arc<dyn ProfileService>>()?.current().await
}

/// [MUTATES STATE] Create a new profile owned by the current user.
#[orca_tool(domain = "namespace", verb = "create")]
async fn profile_create(
    args: ProfileCreateArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileDetail> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .create(&args.name, args.description.as_deref())
        .await
}

/// [MUTATES STATE] Delete a profile (owner only).
#[orca_tool(domain = "namespace", verb = "delete")]
async fn profile_delete(
    args: ProfileSpecArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .delete(&args.spec)
        .await
}

/// [MUTATES STATE] Set the active profile for the current user.
#[orca_tool(domain = "namespace", verb = "use")]
async fn profile_use(
    args: ProfileSpecArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .use_profile(&args.spec)
        .await
}

/// [MUTATES STATE] Share a profile with another user.
#[orca_tool(domain = "namespace.share", verb = "create")]
async fn profile_share_create(
    args: ProfileShareArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .share(&args.spec, &args.user, &args.role)
        .await
}

/// [MUTATES STATE] Remove a share from a profile.
#[orca_tool(domain = "namespace.share", verb = "delete")]
async fn profile_share_delete(
    args: ProfileUnshareArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .unshare(&args.spec, &args.user)
        .await
}

/// List shares on a profile (owner only).
#[orca_tool(domain = "namespace.share", verb = "list")]
async fn profile_share_list(
    args: ProfileSpecArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ProfileSharesReport> {
    ctx.service::<Arc<dyn ProfileService>>()?
        .shares(&args.spec)
        .await
}

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

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

/// Embedder hook — see `services::mod` doc.
pub trait ProvideProfile {
    fn profile(&self) -> std::sync::Arc<dyn ProfileService>;
}

/// Register a `ProfileService` into `ToolCtx`.
pub fn register_profile(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideProfile) {
    ctx.register_service(p.profile());
}
