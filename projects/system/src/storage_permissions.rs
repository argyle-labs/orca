//! Storage permission-drift diagnostics — Slice 1: surface-only.
//!
//! A [`Health::WriteDenied`](plugin_toolkit::storage::Health::WriteDenied) share
//! is live and readable but denies writes because its SERVER-SIDE mode/owner
//! drifted (the immich upload-loop class: a share that slipped 777→775 so the
//! mounting identity lands on "other" without write). orca already detects this
//! and persists it on the placement's `health`, but deliberately stops short of
//! remediating — the fix must run on the host that *serves* the share, not the
//! one that mounts it, and a remount can't fix a server-side perm drift.
//!
//! This provider surfaces every such placement through `diagnostics.diagnose`
//! with a [`RepairSpec`] describing the server-side fix. It performs **no**
//! privileged action: the actual server-side chmod/chown repair
//! (confirm-required, allowlisted to `/mnt/user/<share>/**`, mode matched to the
//! share's healthy siblings) is a later slice. Here the repair is suggest-only,
//! so the condition becomes visible fleet-wide with zero new privileged surface.

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
    let detail = format!(
        "Mount {target} (from {src}) is live and readable but writes are denied \
         (EACCES). This is server-side permission drift: the share's mode/owner on \
         {server} changed so the orca mount identity lost write access — reads still \
         pass, so plain liveness looks healthy. A remount cannot fix it. The fix runs \
         on {server} (the host serving the share): restore write for the mount \
         identity on {path}, matching a healthy sibling share's mode/owner (e.g. the \
         media share). Automated repair is not yet available; apply it manually for now."
    );
    let repair = RepairSpec {
        id: repair_id(target),
        description: format!(
            "Restore write access on {server}:{path} to match the share's healthy \
             siblings (server-side chmod/chown). Not yet automated — run manually."
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

/// Slice-1 repair: performs nothing privileged. Reports that automated
/// remediation is not yet available and points at the manual server-side fix.
/// (Slice 2 replaces this body with the confirm-gated, allowlisted mesh chmod.)
fn plan_only(args: &RepairArgs) -> RepairOutcome {
    RepairOutcome {
        id: args.repair_id.clone(),
        provider: PROVIDER.to_string(),
        ok: false,
        message: "Automated server-side permission repair is not yet available. \
                  To fix now, on the host serving this share restore write access \
                  for the orca mount identity on the share's server-side path \
                  (chmod/chown to match a healthy sibling share)."
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
        assert!(!out.ok, "slice 1 performs no privileged action");
        assert_eq!(out.id, "write-denied:/mnt/data/photos");
        assert!(out.message.contains("not yet available"));
    }
}
