//! Generic storage tool surface.
//!
//! orca does not care *what kind* of storage a provider is — NFS, SMB,
//! Proxmox-managed disk — only that it has access to storage and what that
//! storage can do. These verbs iterate the process-global `storage` registry
//! ([`plugin_toolkit::storage`]) that each adapter plugin registers itself
//! against at bootstrap, rather than naming any backend by type:
//!
//! * `storage.list`    — every registered provider + its capabilities
//! * `storage.shares`  — enumerate shares/volumes across backends (optional filter)
//! * `storage.unmount` — unmount a target on a named backend
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use derive::orca_tool;
use plugin_toolkit::storage::{self, Capability, MountOutcome, Provider};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── list ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageListArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageListOutput {
    pub providers: Vec<Provider>,
}

/// Every storage backend registered with this daemon, with the capabilities
/// each advertises. Empty before any storage adapter has bootstrapped.
#[orca_tool(domain = "storage", verb = "list")]
async fn storage_list(
    _args: StorageListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageListOutput> {
    Ok(StorageListOutput {
        providers: storage::providers(),
    })
}

// ── shares ───────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageSharesArgs {
    /// Restrict to a single backend by provider name. Empty = all backends
    /// that advertise the `list` capability.
    #[arg(long)]
    pub provider: Option<String>,
}

/// A share/volume tagged with the backend that exposes it. Flat projection of
/// [`plugin_toolkit::storage::Share`] so consumers don't depend on the domain type.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ShareRow {
    pub provider: String,
    pub id: String,
    pub source: String,
    pub target: Option<String>,
    pub fstype: String,
    pub mounted: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageSharesOutput {
    pub shares: Vec<ShareRow>,
    /// Per-backend enumeration errors (non-fatal), keyed by provider name, so a
    /// single unreachable backend doesn't blank the whole listing.
    pub errors: Vec<StorageBackendError>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageBackendError {
    pub provider: String,
    pub error: String,
}

/// Enumerate shares/volumes across registered backends. Backends that don't
/// advertise `list` are skipped; per-backend failures are collected into
/// `errors` rather than failing the whole call.
#[orca_tool(domain = "storage", verb = "shares")]
async fn storage_shares(
    args: StorageSharesArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageSharesOutput> {
    let mut shares = Vec::new();
    let mut errors = Vec::new();
    for b in storage::backends() {
        if let Some(want) = args.provider.as_deref()
            && b.name() != want
        {
            continue;
        }
        if !b.supports(Capability::List) {
            continue;
        }
        match b.list_shares().await {
            Ok(found) => shares.extend(found.into_iter().map(|s| ShareRow {
                provider: b.name().to_string(),
                id: s.id,
                source: s.source,
                target: s.target,
                fstype: s.fstype,
                mounted: s.mounted,
            })),
            Err(e) => errors.push(StorageBackendError {
                provider: b.name().to_string(),
                error: e.to_string(),
            }),
        }
    }
    Ok(StorageSharesOutput { shares, errors })
}

// ── unmount ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageUnmountArgs {
    /// Backend provider name (e.g. `nfs`, `smb`).
    #[arg(long)]
    pub provider: String,
    /// Mount target to release.
    #[arg(long)]
    pub target: String,
}

/// Unmount a target on a named backend. Errors if the provider is unknown or
/// does not advertise the `unmount` capability.
#[orca_tool(domain = "storage", verb = "unmount")]
async fn storage_unmount(
    args: StorageUnmountArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<MountOutcome> {
    let b = storage::backend(&args.provider)
        .ok_or_else(|| anyhow::anyhow!("no storage backend named `{}`", args.provider))?;
    if !b.supports(Capability::Unmount) {
        anyhow::bail!("backend `{}` does not support unmount", args.provider);
    }
    Ok(b.unmount(&args.target).await?)
}
