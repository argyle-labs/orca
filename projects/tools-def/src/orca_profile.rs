//! Profile registry — list/show/current/create/delete/use/share/unshare/shares.
//!
//! v1 single-user always operates on `LOCAL_USER`; multi-tenant arrives by
//! swapping the `ProfileService` impl, not by changing the surface.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

// ── Shared rows ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub is_active: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileListReport {
    pub profiles: Vec<ProfileSummary>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCurrentReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ProfileSummary>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileMutationResult {
    pub id: String,
    pub name: String,
    pub changed: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShareEntry {
    pub user_id: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileSharesReport {
    pub profile_id: String,
    pub shares: Vec<ProfileShareEntry>,
}

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileListArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShowArgs {
    /// Profile id or name. Omit to show the active profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCurrentArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileCreateArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileSpecArgs {
    /// Profile id or name.
    pub spec: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileShareArgs {
    pub spec: String,
    pub user: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProfileUnshareArgs {
    pub spec: String,
    pub user: String,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn profile_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::profile::ProfileService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::profile::ProfileService>>()
}

/// List all profiles the current user can access (owned + shared).
#[orca_tool(domain = "profile", verb = "list")]
async fn profile_list(
    _args: ProfileListArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileListReport> {
    profile_svc(ctx)?.list().await
}

/// Show details of a profile (defaults to the active one).
#[orca_tool(domain = "profile", verb = "show")]
async fn profile_show(
    args: ProfileShowArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileDetail> {
    profile_svc(ctx)?.show(args.spec.as_deref()).await
}

/// Show the currently active profile (or None).
#[orca_tool(domain = "profile", verb = "current")]
async fn profile_current(
    _args: ProfileCurrentArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileCurrentReport> {
    profile_svc(ctx)?.current().await
}

/// [MUTATES STATE] Create a new profile owned by the current user.
#[orca_tool(domain = "profile", verb = "create")]
async fn profile_create(
    args: ProfileCreateArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileDetail> {
    profile_svc(ctx)?
        .create(&args.name, args.description.as_deref())
        .await
}

/// [MUTATES STATE] Delete a profile (owner only).
#[orca_tool(domain = "profile", verb = "delete")]
async fn profile_delete(
    args: ProfileSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    profile_svc(ctx)?.delete(&args.spec).await
}

/// [MUTATES STATE] Set the active profile for the current user.
#[orca_tool(domain = "profile", verb = "use")]
async fn profile_use(
    args: ProfileSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    profile_svc(ctx)?.use_profile(&args.spec).await
}

/// [MUTATES STATE] Share a profile with another user.
#[orca_tool(domain = "profile", verb = "share")]
async fn profile_share(
    args: ProfileShareArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    profile_svc(ctx)?
        .share(&args.spec, &args.user, &args.role)
        .await
}

/// [MUTATES STATE] Remove a share from a profile.
#[orca_tool(domain = "profile", verb = "unshare")]
async fn profile_unshare(
    args: ProfileUnshareArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileMutationResult> {
    profile_svc(ctx)?.unshare(&args.spec, &args.user).await
}

/// List sharees on a profile (owner only).
#[orca_tool(domain = "profile", verb = "shares")]
async fn profile_shares(
    args: ProfileSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ProfileSharesReport> {
    profile_svc(ctx)?.shares(&args.spec).await
}
