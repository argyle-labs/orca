//! Profile registry — list/show/current/create/delete/use/share/unshare/shares.
//!
//! v1 single-user always operates on `LOCAL_USER`; multi-tenant arrives by
//! swapping the `ProfileService` impl, not by changing the surface.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

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

// ── Tool defs ───────────────────────────────────────────────────────────────

pub struct ProfileList;
impl OrcaToolDef for ProfileList {
    const NAME: &'static str = "profile.list";
    const DESCRIPTION: &'static str =
        "List all profiles the current user can access (owned + shared).";
    type Args = ProfileListArgs;
    type Output = ProfileListReport;
}
pub struct ProfileShow;
impl OrcaToolDef for ProfileShow {
    const NAME: &'static str = "profile.show";
    const DESCRIPTION: &'static str = "Show details of a profile (defaults to the active one).";
    type Args = ProfileShowArgs;
    type Output = ProfileDetail;
}
pub struct ProfileCurrent;
impl OrcaToolDef for ProfileCurrent {
    const NAME: &'static str = "profile.current";
    const DESCRIPTION: &'static str = "Show the currently active profile (or None).";
    type Args = ProfileCurrentArgs;
    type Output = ProfileCurrentReport;
}
pub struct ProfileCreate;
impl OrcaToolDef for ProfileCreate {
    const NAME: &'static str = "profile.create";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Create a new profile owned by the current user.";
    type Args = ProfileCreateArgs;
    type Output = ProfileDetail;
}
pub struct ProfileDelete;
impl OrcaToolDef for ProfileDelete {
    const NAME: &'static str = "profile.delete";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Delete a profile (owner only).";
    type Args = ProfileSpecArgs;
    type Output = ProfileMutationResult;
}
pub struct ProfileUse;
impl OrcaToolDef for ProfileUse {
    const NAME: &'static str = "profile.use";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Set the active profile for the current user.";
    type Args = ProfileSpecArgs;
    type Output = ProfileMutationResult;
}
pub struct ProfileShare;
impl OrcaToolDef for ProfileShare {
    const NAME: &'static str = "profile.share";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Share a profile with another user.";
    type Args = ProfileShareArgs;
    type Output = ProfileMutationResult;
}
pub struct ProfileUnshare;
impl OrcaToolDef for ProfileUnshare {
    const NAME: &'static str = "profile.unshare";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove a share from a profile.";
    type Args = ProfileUnshareArgs;
    type Output = ProfileMutationResult;
}
pub struct ProfileShares;
impl OrcaToolDef for ProfileShares {
    const NAME: &'static str = "profile.shares";
    const DESCRIPTION: &'static str = "List sharees on a profile (owner only).";
    type Args = ProfileSpecArgs;
    type Output = ProfileSharesReport;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::profile::ProfileService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn ProfileService>> {
        ctx.service::<Arc<dyn ProfileService>>()
    }

    #[async_trait]
    impl OrcaTool for ProfileList {
        async fn run(_a: ProfileListArgs, ctx: &ToolCtx) -> Result<ProfileListReport> {
            svc(ctx)?.list().await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileShow {
        async fn run(a: ProfileShowArgs, ctx: &ToolCtx) -> Result<ProfileDetail> {
            svc(ctx)?.show(a.spec.as_deref()).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileCurrent {
        async fn run(_a: ProfileCurrentArgs, ctx: &ToolCtx) -> Result<ProfileCurrentReport> {
            svc(ctx)?.current().await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileCreate {
        async fn run(a: ProfileCreateArgs, ctx: &ToolCtx) -> Result<ProfileDetail> {
            svc(ctx)?.create(&a.name, a.description.as_deref()).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileDelete {
        async fn run(a: ProfileSpecArgs, ctx: &ToolCtx) -> Result<ProfileMutationResult> {
            svc(ctx)?.delete(&a.spec).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileUse {
        async fn run(a: ProfileSpecArgs, ctx: &ToolCtx) -> Result<ProfileMutationResult> {
            svc(ctx)?.use_profile(&a.spec).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileShare {
        async fn run(a: ProfileShareArgs, ctx: &ToolCtx) -> Result<ProfileMutationResult> {
            svc(ctx)?.share(&a.spec, &a.user, &a.role).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileUnshare {
        async fn run(a: ProfileUnshareArgs, ctx: &ToolCtx) -> Result<ProfileMutationResult> {
            svc(ctx)?.unshare(&a.spec, &a.user).await
        }
    }
    #[async_trait]
    impl OrcaTool for ProfileShares {
        async fn run(a: ProfileSpecArgs, ctx: &ToolCtx) -> Result<ProfileSharesReport> {
            svc(ctx)?.shares(&a.spec).await
        }
    }
}
