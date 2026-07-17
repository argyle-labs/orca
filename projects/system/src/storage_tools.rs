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
//! * `storage.mount`   — render the declared `managed_mounts` into autofs + reload
//! * `storage.recover` — self-heal stale autofs mounts (force-release + re-trigger)
//! * `storage.unmount` — unmount a target on a named backend
//!
//! Dispatched through the single daemon handler so CLI / REST / MCP / UI share
//! one path ([[feedback-cli-api-mcp-one-path]]).

use derive::orca_tool;
use plugin_toolkit::storage::{self, Capability, MountOutcome, Provider, Usage};
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

// ── mount ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageMountArgs {
    /// After rendering, immediately trigger each declared mountpoint (a direct
    /// autofs map mounts on access) so shares come up now rather than on first
    /// consumer access. Defaults to true.
    #[arg(long)]
    pub trigger: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageMountOutput {
    /// Number of enabled network-share mounts rendered into the autofs map.
    pub rendered: usize,
    /// Config files that changed this run (the drift set). Empty = host already
    /// matched the declared store.
    pub changed: Vec<String>,
    /// Whether autofs was reloaded (only when something changed).
    pub reloaded: bool,
    /// Mountpoints accessed to force an immediate mount (when `trigger`).
    pub triggered: Vec<String>,
    /// Userspace-process (object-store/FUSE) mounts brought up this run, via the
    /// owning backend's helper process rather than autofs.
    pub userspace_mounted: Vec<String>,
    /// Userspace-process mounts torn down this run (disabled rows).
    pub userspace_unmounted: Vec<String>,
    /// Non-fatal errors during apply/trigger.
    pub errors: Vec<String>,
}

/// Render every enabled network-share entry in the `managed_mounts` store into
/// the orca autofs direct map and reload autofs. autofs then owns on-demand
/// mounting, idle unmount, and ordered-source (primary → failover) failover;
/// the `storage.recover_stale` loop covers the one case autofs can't self-heal
/// (an actively-held stale hard mount). Idempotent — a run that changes nothing
/// neither rewrites files nor reloads autofs.
#[orca_tool(domain = "storage", verb = "mount")]
async fn storage_mount(
    args: StorageMountArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageMountOutput> {
    let mounts = crate::managed_mounts::endpoint_db::list()?;
    let rendered = crate::autofs::render_map(&mounts)
        .lines()
        .filter(|l| !l.starts_with('#'))
        .count();

    // Kernel-mount (nfs/smb) path — UNCHANGED. autofs owns these; the renderer
    // already filters to `kind == "network_share"`, and userspace-process mounts
    // (object stores) never enter the map, so this call is unaffected by them.
    let applied = crate::autofs::apply(&mounts).await;

    let mut triggered = Vec::new();
    let mut errors = applied.errors;
    if args.trigger.unwrap_or(true) {
        let targets: Vec<String> = mounts
            .iter()
            .filter(|m| m.enabled && m.kind == "network_share")
            .map(|m| m.target.clone())
            .collect();
        errors.extend(crate::autofs::trigger(&targets).await);
        triggered = targets;
    }

    // Userspace-process (object-store/FUSE) path — driven through the backend's
    // helper, NOT autofs. Branches on the backend's `mount_style` per row; a
    // kernel-mount row is skipped here (and vice-versa above), so the two paths
    // never overlap.
    let usp = crate::userspace_mounts::reconcile(&mounts).await;
    errors.extend(usp.errors);

    Ok(StorageMountOutput {
        rendered,
        changed: applied.changed,
        reloaded: applied.reloaded,
        triggered,
        userspace_mounted: usp.mounted,
        userspace_unmounted: usp.unmounted,
        errors,
    })
}

// ── recover ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StorageRecoverArgs {
    /// Per-target liveness-probe timeout in seconds. A mount whose `stat` hangs
    /// past this is treated as stale. Defaults to 5.
    #[arg(long)]
    pub health_timeout_secs: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StorageRecoverOutput {
    pub recovered: Vec<String>,
    pub still_stale: Vec<String>,
    pub healthy: Vec<String>,
    pub errors: Vec<String>,
    pub no_stale_found: bool,
}

/// Self-heal stale autofs mounts across the declared network shares — the one
/// failure mode autofs can't recover itself (an actively-held stale `hard`
/// mount). Probes each declared target; a stale one is force-released and
/// re-accessed so autofs remounts + fails over to the next ordered source.
/// This is what the periodic self-heal schedule invokes per host.
#[orca_tool(domain = "storage", verb = "recover")]
async fn storage_recover(
    args: StorageRecoverArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<StorageRecoverOutput> {
    let targets: Vec<String> = crate::managed_mounts::endpoint_db::list()?
        .into_iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .map(|m| m.target)
        .collect();

    let timeout = std::time::Duration::from_secs(args.health_timeout_secs.unwrap_or(5));
    let r = crate::autofs::recover(&targets, timeout).await;

    Ok(StorageRecoverOutput {
        recovered: r.recovered,
        still_stale: r.still_stale,
        healthy: r.healthy,
        errors: r.errors,
        no_stale_found: r.no_stale_found,
    })
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

// ── usage ────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsageArgs {
    /// Backend provider name (e.g. an object store).
    #[arg(long)]
    pub provider: String,
    /// Share/volume id to report usage for (`s3://bucket/prefix`, …).
    #[arg(long)]
    pub id: String,
}

/// Capacity/usage for a volume on a named backend. Errors if the provider is
/// unknown or does not advertise the `usage` capability. Object stores that
/// cannot report usage return a documented stub from the backend — this verb
/// surfaces whatever the backend implements without special-casing any kind.
#[orca_tool(domain = "storage", verb = "usage")]
async fn storage_usage(args: StorageUsageArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<Usage> {
    let b = storage::backend(&args.provider)
        .ok_or_else(|| anyhow::anyhow!("no storage backend named `{}`", args.provider))?;
    if !b.supports(Capability::Usage) {
        anyhow::bail!("backend `{}` does not support usage", args.provider);
    }
    Ok(b.usage(&args.id).await?)
}

#[cfg(test)]
#[allow(clippy::disallowed_types)] // tests build serde_json::Value fixtures directly
mod tests {
    use super::*;

    #[test]
    fn list_args_default_deserializes_from_empty() {
        let a: StorageListArgs = serde_json::from_str("{}").unwrap();
        let _ = a; // no fields; just proves default/serde wiring
        let a2 = StorageListArgs::default();
        let _ = a2;
    }

    #[test]
    fn shares_args_provider_optional_defaults_none() {
        let a: StorageSharesArgs = serde_json::from_str("{}").unwrap();
        assert!(a.provider.is_none());
        let a2 = StorageSharesArgs::default();
        assert!(a2.provider.is_none());
    }

    #[test]
    fn shares_args_camel_case_provider() {
        let a: StorageSharesArgs = serde_json::from_str(r#"{"provider":"nfs"}"#).unwrap();
        assert_eq!(a.provider.as_deref(), Some("nfs"));
    }

    #[test]
    fn mount_args_trigger_optional_defaults_none() {
        let a: StorageMountArgs = serde_json::from_str("{}").unwrap();
        assert!(a.trigger.is_none());
        // the tool treats None as true
        assert!(a.trigger.unwrap_or(true));
        let explicit: StorageMountArgs = serde_json::from_str(r#"{"trigger":false}"#).unwrap();
        assert_eq!(explicit.trigger, Some(false));
        assert!(!explicit.trigger.unwrap_or(true));
    }

    #[test]
    fn recover_args_timeout_optional_defaults_none() {
        let a: StorageRecoverArgs = serde_json::from_str("{}").unwrap();
        assert!(a.health_timeout_secs.is_none());
        // the tool default is 5s
        assert_eq!(a.health_timeout_secs.unwrap_or(5), 5);
        let explicit: StorageRecoverArgs =
            serde_json::from_str(r#"{"healthTimeoutSecs":12}"#).unwrap();
        assert_eq!(explicit.health_timeout_secs, Some(12));
    }

    #[test]
    fn unmount_args_require_provider_and_target() {
        let a: StorageUnmountArgs =
            serde_json::from_str(r#"{"provider":"smb","target":"/mnt/media"}"#).unwrap();
        assert_eq!(a.provider, "smb");
        assert_eq!(a.target, "/mnt/media");
        // both fields are required (no default) — missing one is an error
        assert!(serde_json::from_str::<StorageUnmountArgs>(r#"{"provider":"smb"}"#).is_err());
    }

    #[test]
    fn share_row_serializes_camel_case() {
        let row = ShareRow {
            provider: "nfs".into(),
            id: "export1".into(),
            source: "host:/export".into(),
            target: Some("/mnt/x".into()),
            fstype: "nfs4".into(),
            mounted: true,
        };
        let v: serde_json::Value = serde_json::to_value(&row).unwrap();
        assert_eq!(v["provider"], "nfs");
        assert_eq!(v["id"], "export1");
        assert_eq!(v["source"], "host:/export");
        assert_eq!(v["target"], "/mnt/x");
        assert_eq!(v["fstype"], "nfs4");
        assert_eq!(v["mounted"], true);
    }

    #[test]
    fn share_row_null_target_roundtrips() {
        let row = ShareRow {
            provider: "smb".into(),
            id: "s".into(),
            source: "//nas/s".into(),
            target: None,
            fstype: "cifs".into(),
            mounted: false,
        };
        let v: serde_json::Value = serde_json::to_value(&row).unwrap();
        assert!(v["target"].is_null());
    }

    #[test]
    fn backend_error_serializes() {
        let e = StorageBackendError {
            provider: "nfs".into(),
            error: "boom".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&e).unwrap();
        assert_eq!(v["provider"], "nfs");
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn shares_output_shape() {
        let out = StorageSharesOutput {
            shares: vec![],
            errors: vec![StorageBackendError {
                provider: "nfs".into(),
                error: "down".into(),
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(v["shares"].as_array().unwrap().is_empty());
        assert_eq!(v["errors"][0]["provider"], "nfs");
    }

    #[test]
    fn mount_output_serializes_camel_case() {
        let out = StorageMountOutput {
            rendered: 3,
            changed: vec!["/etc/auto.orca".into()],
            reloaded: true,
            triggered: vec!["/mnt/a".into()],
            userspace_mounted: vec!["/mnt/obj".into()],
            userspace_unmounted: vec![],
            errors: vec![],
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["rendered"], 3);
        assert_eq!(v["changed"][0], "/etc/auto.orca");
        assert_eq!(v["reloaded"], true);
        assert_eq!(v["triggered"][0], "/mnt/a");
        assert_eq!(v["userspaceMounted"][0], "/mnt/obj");
        assert!(v["userspaceUnmounted"].as_array().unwrap().is_empty());
        assert!(v["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn recover_output_serializes_camel_case() {
        let out = StorageRecoverOutput {
            recovered: vec!["/mnt/a".into()],
            still_stale: vec![],
            healthy: vec!["/mnt/b".into()],
            errors: vec![],
            no_stale_found: false,
        };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert_eq!(v["recovered"][0], "/mnt/a");
        assert!(v["stillStale"].as_array().unwrap().is_empty());
        assert_eq!(v["healthy"][0], "/mnt/b");
        assert_eq!(v["noStaleFound"], false);
    }

    #[test]
    fn list_output_serializes() {
        let out = StorageListOutput { providers: vec![] };
        let v: serde_json::Value = serde_json::to_value(&out).unwrap();
        assert!(v["providers"].as_array().unwrap().is_empty());
    }
}
