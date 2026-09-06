//! Storage permission-drift diagnostics — surfaces write-denied shares.
//!
//! A [`Health::WriteDenied`](plugin_toolkit::storage::Health::WriteDenied) share
//! is live and readable but denies writes because its SERVER-SIDE mode/owner
//! drifted (the immich upload-loop class: a share that slipped 777→775 so the
//! mounting identity lands on "other" without write). orca detects this and
//! persists it on the placement's `health`; a remount can't fix it because the
//! drift is on the host that *serves* the share, not the one that mounts it.
//!
//! This provider surfaces every such placement through `diagnostics.diagnose`
//! with a [`RepairSpec`] carrying the exact repair commands. It performs **no**
//! privileged action itself — execution lives in the peer-dispatchable
//! `storage.share.repair-permissions` tool (a DiagnosticsProvider has no
//! `ToolCtx` to reach the mesh transport). That tool detects a candidate mode
//! from what sibling shares use, and applies an explicitly-confirmed mode via the
//! allowlisted `SetShareMode` privileged op — confirm-required, never a guess.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use contract::BoxFuture;
use contract::diagnostics::{
    DiagnoseArgs, DiagnosticsProvider, Finding, RepairArgs, RepairOutcome, RepairSpec, Severity,
    register_provider,
};
use plugin_toolkit::route::Route;
use plugin_toolkit::storage::Health;

use crate::mount_converge::source_of_route;
use crate::{host_identity, mounts, shares};

/// Registry name for this provider (the `diagnostics.diagnose` `provider` field).
const PROVIDER: &str = "storage-permissions";

/// Register the core-owned storage-permissions diagnostics provider. Called once
/// at daemon boot, alongside the other builtin providers.
pub fn register() {
    register_provider(Arc::new(StoragePermissions));
}

struct StoragePermissions;

impl DiagnosticsProvider for StoragePermissions {
    fn name(&self) -> &str {
        PROVIDER
    }

    fn diagnose(&self, _args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        // Pure DB reads; no awaiting needed, but the trait is async.
        Box::pin(async move { Ok(scan()) })
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        Box::pin(async move { Ok(plan_only(&args)) })
    }
}

/// Scan this host's own placements for write-denied shares and build a finding
/// for each. Only the owning host's `health` column is truth (it is host-local
/// and never replicated), so we filter to placements this host owns.
fn scan() -> Vec<Finding> {
    let me = host_identity::machine_id();
    let placements = match mounts::endpoint_db::list() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let shares: HashMap<String, shares::EndpointRow> = shares::endpoint_db::list()
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.id.clone(), s))
        .collect();

    placements
        .into_iter()
        .filter(|ep| ep.host == me && ep.health == Health::WriteDenied)
        .map(|ep| {
            let share = shares.get(&ep.share_id);
            let (server, server_path) = resolve_server_path(&ep, share);
            build_finding(
                &ep.target,
                ep.active_route.as_deref(),
                &server,
                server_path.as_deref(),
            )
        })
        .collect()
}

/// Resolve the (server host, server-side path) a placement's write-denied source
/// points at. Prefers the share route whose rendered source matches the source
/// this placement is actually mounted from (`active_route`); falls back to the
/// first enabled route that carries an export path. Returns `("unknown", None)`
/// when nothing resolves so the finding still surfaces (visibility beats silence).
fn resolve_server_path(
    ep: &mounts::EndpointRow,
    share: Option<&shares::EndpointRow>,
) -> (String, Option<String>) {
    let Some(share) = share else {
        return ("unknown".to_string(), None);
    };
    let route = pick_route(&share.fstype, ep.active_route.as_deref(), &share.routes);
    match route {
        Some(r) => (r.value.clone(), r.path.clone()),
        None => ("unknown".to_string(), None),
    }
}

/// Pick the route a write-denied placement resolves through. If `active_route`
/// (the source last mounted) matches a route's rendered source, that is the
/// exact one; otherwise the first enabled route with an export path — the most
/// likely server-side target when the live source is unknown.
fn pick_route<'a>(
    fstype: &str,
    active_route: Option<&str>,
    routes: &'a [Route],
) -> Option<&'a Route> {
    if let Some(active) = active_route
        && let Some(r) = routes
            .iter()
            .find(|r| r.path.is_some() && source_of_route(fstype, r) == active)
    {
        return Some(r);
    }
    routes.iter().find(|r| r.enabled && r.path.is_some())
}

/// Build the finding + suggest-only repair for one write-denied placement.
fn build_finding(
    target: &str,
    source: Option<&str>,
    server: &str,
    server_path: Option<&str>,
) -> Finding {
    let path = server_path.unwrap_or("<share path>");
    let src = source.unwrap_or("<source>");
    // The ready-to-run repair: a dry-run first (detect candidates from sibling
    // shares), then apply the confirmed mode. `--peer {server}` runs it on the
    // host that serves the share, where /mnt/user lives.
    let detect_cmd = format!("storage.share.repair-permissions --peer {server} --path {path}");
    let apply_cmd = format!(
        "storage.share.repair-permissions --peer {server} --path {path} --apply --mode <octal>"
    );
    let detail = format!(
        "Mount {target} (from {src}) is live and readable but writes are denied \
         (EACCES). This is server-side permission drift: the share's mode/owner on \
         {server} changed so the orca mount identity lost write access — reads still \
         pass, so plain liveness looks healthy. A remount cannot fix it.\n\n\
         Detect the right mode (what sibling shares use):\n  {detect_cmd}\n\
         Then apply the confirmed mode:\n  {apply_cmd}"
    );
    let repair = RepairSpec {
        id: repair_id(target),
        description: format!(
            "Restore write access on {server}:{path}. Detect candidates: `{detect_cmd}`; \
             then apply a confirmed mode: `{apply_cmd}`."
        ),
        automatic: false,
        privileged: true,
        delegate: None,
    };
    Finding {
        id: repair_id(target),
        provider: PROVIDER.to_string(),
        // Degraded, not broken: the share still serves reads.
        severity: Severity::Warn,
        title: format!("Share write-denied (permission drift): {target}"),
        detail,
        repair: Some(repair),
    }
}

/// Stable per-placement repair id (unique within this provider).
fn repair_id(target: &str) -> String {
    format!("write-denied:{target}")
}

/// The provider's `repair` entry. Execution lives in the peer-dispatchable
/// `storage.share.repair-permissions` tool (it needs the mesh transport, which a
/// DiagnosticsProvider has no `ToolCtx` to reach), so this points the caller at
/// that tool's detect→confirm→apply flow rather than acting here. The repair
/// stays confirm-required: the admin runs the tool, choosing an explicit mode.
fn plan_only(args: &RepairArgs) -> RepairOutcome {
    RepairOutcome {
        id: args.repair_id.clone(),
        provider: PROVIDER.to_string(),
        ok: false,
        message: "Run `storage.share.repair-permissions --peer <serving-host> --path \
                  <server-path>` to detect the mode sibling shares use, then re-run with \
                  `--apply --mode <octal>` to apply the confirmed mode. The finding's \
                  detail carries the exact commands. (Execution is that admin tool, not \
                  diagnostics.repair, because the fix runs on the serving host over the \
                  mesh.)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(value: &str, path: Option<&str>, enabled: bool) -> Route {
        Route {
            kind: "lan_v4".to_string(),
            scheme: None,
            value: value.to_string(),
            port: None,
            path: path.map(str::to_string),
            enabled,
            source: None,
            kind_label: None,
            last_seen_at: None,
        }
    }

    #[test]
    fn pick_route_prefers_active_source_match() {
        // Two enabled routes with paths; active_route names the second's source.
        let routes = vec![
            route("10.0.0.10", Some("/mnt/user/data"), true),
            route("10.0.0.11", Some("/mnt/user/data"), true),
        ];
        // NFS source shape: host:/export.
        let active = source_of_route("nfs4", &routes[1]);
        let got = pick_route("nfs4", Some(&active), &routes).unwrap();
        assert_eq!(got.value, "10.0.0.11");
    }

    #[test]
    fn pick_route_falls_back_to_first_enabled_with_path() {
        let routes = vec![
            route("10.0.0.10", None, true), // no export path — skip
            route("10.0.0.11", Some("/mnt/user/data"), false), // disabled — skip
            route("10.0.0.12", Some("/mnt/user/data"), true), // first eligible
        ];
        let got = pick_route("nfs4", None, &routes).unwrap();
        assert_eq!(got.value, "10.0.0.12");
    }

    #[test]
    fn pick_route_none_when_no_path_route() {
        let routes = vec![route("10.0.0.10", None, true)];
        assert!(pick_route("nfs4", None, &routes).is_none());
    }

    #[test]
    fn build_finding_is_warn_with_privileged_suggest_only_repair() {
        let f = build_finding(
            "/mnt/data/photos",
            Some("//10.0.0.10/data"),
            "10.0.0.10",
            Some("/mnt/user/data"),
        );
        assert_eq!(f.provider, PROVIDER);
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.id, "write-denied:/mnt/data/photos");
        let r = f.repair.expect("finding carries a repair");
        assert!(!r.automatic, "server-side fix must require confirmation");
        assert!(r.privileged, "server-side fix needs privilege");
        assert!(r.delegate.is_none());
        // The server + server-side path are surfaced so an operator (or Slice 2)
        // knows exactly where to act.
        assert!(f.detail.contains("10.0.0.10"));
        assert!(f.detail.contains("/mnt/user/data"));
    }

    #[test]
    fn plan_only_reports_not_yet_automated_without_acting() {
        let out = plan_only(&RepairArgs {
            provider: PROVIDER.to_string(),
            repair_id: "write-denied:/mnt/data/photos".to_string(),
            confirm: true,
        });
        assert!(!out.ok, "the provider itself performs no privileged action");
        assert_eq!(out.id, "write-denied:/mnt/data/photos");
        assert!(out.message.contains("storage.share.repair-permissions"));
    }
}
