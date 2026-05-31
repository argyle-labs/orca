//! `namespace` — per-user, shareable workspaces. Each namespace owns
//! filesystem content (`~/.orca/profiles/<id>/`) and metadata in `orca.db`.
//! One user has many namespaces; namespaces can be shared with other users
//! in `viewer` or `collaborator` roles.
//!
//! v1 is single-machine. The data model is federation-ready (UUID ids,
//! set-shaped ACLs, file-granular content) so the pod mesh sync layer can
//! replicate without re-shaping.
//!
//! No service trait — tools call the free functions in `native` directly
//! per [[feedback_no_indirection]].
//!
//! Moved from `platform::profile` + `platform::profile_native` + `platform::profile_manager`
//! in slice 3 of crate-topology-v2 (2026-05-27).

use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod doc_registry;
mod manager;
mod native;
mod spec;

pub use manager::{Access, Namespace, NamespaceManager, Role};

// ── Shared rows ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct NamespaceSummary {
    pub id: String,
    pub name: String,
    pub owner_user_id: String,
    pub is_active: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct NamespaceDetail {
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
pub struct NamespaceListReport {
    pub namespaces: Vec<NamespaceSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceCurrentReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<NamespaceSummary>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceMutationResult {
    pub id: String,
    pub name: String,
    pub changed: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceShareEntry {
    pub user_id: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceSharesReport {
    pub namespace_id: String,
    pub shares: Vec<NamespaceShareEntry>,
}

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceListArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceShowArgs {
    /// Namespace id or name. Omit to show the active namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceCurrentArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceCreateArgs {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceSpecArgs {
    /// Namespace id or name.
    pub spec: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceShareArgs {
    pub spec: String,
    pub user: String,
    /// `viewer` | `collaborator`
    pub role: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct NamespaceUnshareArgs {
    pub spec: String,
    pub user: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// Tools — call free fns in `native` directly. No service trait.
// ═══════════════════════════════════════════════════════════════════════════

/// List all namespaces the current user can access (owned + shared).
#[orca_tool(domain = "namespace", verb = "list")]
async fn namespace_list(
    _args: NamespaceListArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceListReport> {
    native::list(&ctx.config).await
}

/// Show details of a namespace (defaults to the active one).
#[orca_tool(domain = "namespace", verb = "detail")]
async fn namespace_show(
    args: NamespaceShowArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceDetail> {
    native::show(&ctx.config, args.spec.as_deref()).await
}

/// Show the currently active namespace (or None).
#[orca_tool(domain = "namespace", verb = "current")]
async fn namespace_current(
    _args: NamespaceCurrentArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceCurrentReport> {
    native::current(&ctx.config).await
}

/// [MUTATES STATE] Create a new namespace owned by the current user.
#[orca_tool(domain = "namespace", verb = "create")]
async fn namespace_create(
    args: NamespaceCreateArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceDetail> {
    native::create(&ctx.config, &args.name, args.description.as_deref()).await
}

/// [MUTATES STATE] Delete a namespace (owner only).
#[orca_tool(domain = "namespace", verb = "delete")]
async fn namespace_delete(
    args: NamespaceSpecArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceMutationResult> {
    native::delete(&ctx.config, &args.spec).await
}

/// [MUTATES STATE] Set the active namespace for the current user.
#[orca_tool(domain = "namespace", verb = "use")]
async fn namespace_use(
    args: NamespaceSpecArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceMutationResult> {
    native::use_namespace(&ctx.config, &args.spec).await
}

/// [MUTATES STATE] Share a namespace with another user.
#[orca_tool(domain = "namespace.share", verb = "create")]
async fn namespace_share_create(
    args: NamespaceShareArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceMutationResult> {
    native::share(&ctx.config, &args.spec, &args.user, &args.role).await
}

/// [MUTATES STATE] Remove a share from a namespace.
#[orca_tool(domain = "namespace.share", verb = "delete")]
async fn namespace_share_delete(
    args: NamespaceUnshareArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceMutationResult> {
    native::unshare(&ctx.config, &args.spec, &args.user).await
}

/// List shares on a namespace (owner only).
#[orca_tool(domain = "namespace.share", verb = "list")]
async fn namespace_share_list(
    args: NamespaceSpecArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<NamespaceSharesReport> {
    native::shares(&ctx.config, &args.spec).await
}
