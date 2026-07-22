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

// ── recover (shared backend-routed helper) ───────────────────────────

/// Merged outcome of a backend-routed recovery sweep. Mirrors
/// [`crate::autofs::RecoverOutcome`] but additionally carries the
/// declared-but-absent remount vecs a [`storage::RecoverOutcome`] reports, so a
/// plugin's consumer-aware sweep (nfs's `consumer:` / `consumer-skipped-*`
/// tagged entries fold into `recovered` / `still_stale`) is surfaced losslessly.
#[derive(Debug, Default, Clone)]
pub struct MergedRecover {
    pub recovered: Vec<String>,
    pub still_stale: Vec<String>,
    pub healthy: Vec<String>,
    pub remounted: Vec<String>,
    pub still_missing: Vec<String>,
    pub errors: Vec<String>,
    pub no_stale_found: bool,
}

/// The routing decision computed by [`plan_recovery`]: which recover-capable
/// backend gets which targets, and which targets fall back to autofs. Pure data
/// (no I/O) so the target→backend routing is unit-testable without a live
/// registry or touching real mounts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// `(backend_name, targets)` for each recover-capable backend, sorted by name.
    pub backend_calls: Vec<(String, Vec<String>)>,
    /// Targets owned by an unknown or non-recover-capable backend — autofs fallback.
    pub fallback_targets: Vec<String>,
}

/// Group targets by their declared backend and split into recover-capable
/// backend invocations vs autofs fallback. `is_recover_capable(name)` reports
/// whether a registered backend of that name advertises `RecoverStale`.
///
/// Attribution is exact: [`ManagedMount::backend`] names the owning backend, so
/// each backend is called with only its own targets (no need to have backends
/// no-op on foreign targets).
pub fn plan_recovery(
    mounts: &[crate::managed_mounts::ManagedMount],
    is_recover_capable: impl Fn(&str) -> bool,
) -> RecoveryPlan {
    use std::collections::BTreeMap;

    let mut by_backend: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in mounts {
        by_backend
            .entry(m.backend.clone())
            .or_default()
            .push(m.target.clone());
    }

    let mut plan = RecoveryPlan::default();
    for (name, targets) in by_backend {
        if is_recover_capable(&name) {
            plan.backend_calls.push((name, targets));
        } else {
            plan.fallback_targets.extend(targets);
        }
    }
    plan
}

/// Route stale-mount recovery through registered storage backends, with an
/// autofs fallback for any target no recover-capable backend owns.
///
/// Each managed mount names its owning backend ([`ManagedMount::backend`]); we
/// group the targets by that name. For every registered backend that advertises
/// [`Capability::RecoverStale`] we invoke `recover_stale(watch, timeout)` with
/// exactly the targets attributed to it — so the nfs plugin's consumer-aware
/// bind-mount self-heal (host-healthy + consumer-stale ESTALE guard, restart of
/// containers pinning a stale superblock) actually runs. Out-of-process plugins
/// are reached transparently via the storage FFI proxy.
///
/// Targets whose backend is unknown or is not recover-capable fall back to
/// [`crate::autofs::recover`] — preserving today's behavior exactly for hosts
/// with no recover-capable backend registered.
///
/// Core never restarts containers itself: the consumer-restart path lives
/// entirely inside the plugin behind its own guard. Core's only job is to call
/// the backend.
pub async fn recover_via_backends(
    mounts: &[crate::managed_mounts::ManagedMount],
    timeout: std::time::Duration,
) -> MergedRecover {
    // Split targets into per-backend recover invocations and autofs fallback,
    // consulting the process-global registry for recover capability.
    let is_recover_capable = |name: &str| {
        storage::backend(name)
            .map(|b| b.capabilities().contains(&Capability::RecoverStale))
            .unwrap_or(false)
    };
    let plan = plan_recovery(mounts, is_recover_capable);

    let mut merged = MergedRecover::default();

    for (backend_name, targets) in plan.backend_calls {
        // Guaranteed present + recover-capable by `plan_recovery`.
        let b = match storage::backend(&backend_name) {
            Some(b) => b,
            None => continue,
        };
        match b.recover_stale(&targets, timeout).await {
            Ok(out) => {
                merged.recovered.extend(out.recovered);
                merged.still_stale.extend(out.still_stale);
                merged.remounted.extend(out.remounted);
                merged.still_missing.extend(out.still_missing);
                merged.errors.extend(out.errors);
            }
            Err(e) => merged
                .errors
                .push(format!("backend `{backend_name}` recover_stale: {e}")),
        }
    }

    let fallback_targets = plan.fallback_targets;
    if !fallback_targets.is_empty() {
        let r = crate::autofs::recover(&fallback_targets, timeout).await;
        merged.recovered.extend(r.recovered);
        merged.still_stale.extend(r.still_stale);
        merged.healthy.extend(r.healthy);
        merged.errors.extend(r.errors);
    }

    merged.no_stale_found = merged.recovered.is_empty()
        && merged.still_stale.is_empty()
        && merged.remounted.is_empty()
        && merged.still_missing.is_empty();
    merged
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
    let mounts: Vec<crate::managed_mounts::ManagedMount> =
        crate::managed_mounts::endpoint_db::list()?
            .into_iter()
            .filter(|m| m.enabled && m.kind == "network_share")
            .collect();

    let timeout = std::time::Duration::from_secs(args.health_timeout_secs.unwrap_or(5));
    let mut r = recover_via_backends(&mounts, timeout).await;

    // Fold the declared-but-absent remount vecs (populated by consumer-aware
    // backends) into the flat recovered/still_stale surface this tool reports.
    r.recovered.append(&mut r.remounted);
    r.still_stale.append(&mut r.still_missing);

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

    fn mm(name: &str, backend: &str) -> crate::managed_mounts::ManagedMount {
        crate::managed_mounts::ManagedMount {
            name: name.into(),
            backend: backend.into(),
            kind: "network_share".into(),
            source: "server1:/export/pool".into(),
            failover_sources: None,
            target: format!("/mnt/{name}"),
            fstype: "nfs4".into(),
            options: None,
            credential: None,
            remount_policy: None,
            addresses: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn plan_routes_recover_capable_backend_and_falls_back_otherwise() {
        let mounts = vec![mm("a", "nfs"), mm("b", "nfs"), mm("c", "smb")];
        // `nfs` is recover-capable; `smb` is not (→ autofs fallback).
        let plan = plan_recovery(&mounts, |name| name == "nfs");
        assert_eq!(
            plan.backend_calls,
            vec![(
                "nfs".to_string(),
                vec!["/mnt/a".to_string(), "/mnt/b".to_string()]
            )]
        );
        assert_eq!(plan.fallback_targets, vec!["/mnt/c".to_string()]);
    }

    #[test]
    fn plan_unknown_backend_falls_back() {
        let mounts = vec![mm("a", "mystery")];
        let plan = plan_recovery(&mounts, |_| false);
        assert!(plan.backend_calls.is_empty());
        assert_eq!(plan.fallback_targets, vec!["/mnt/a".to_string()]);
    }

    #[test]
    fn plan_all_capable_produces_no_fallback() {
        let mounts = vec![mm("a", "nfs"), mm("b", "smb")];
        let plan = plan_recovery(&mounts, |_| true);
        assert!(plan.fallback_targets.is_empty());
        assert_eq!(plan.backend_calls.len(), 2);
        // Deterministic ordering (BTreeMap): nfs before smb.
        assert_eq!(plan.backend_calls[0].0, "nfs");
        assert_eq!(plan.backend_calls[1].0, "smb");
    }

    #[test]
    fn plan_empty_mounts_is_empty() {
        let plan = plan_recovery(&[], |_| true);
        assert_eq!(plan, RecoveryPlan::default());
    }

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
