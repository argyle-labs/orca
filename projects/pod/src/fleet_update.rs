//! Fleet-wide update fan-out (`update.all` → `orca update all`).
//!
//! A single operator action that updates the whole pod: every joined peer's
//! daemon first (PHASE 1), then every installed plugin on every host (PHASE 2).
//!
//! DRY RUN by default — it only probes current→latest per host/plugin and
//! reports whether an update is available. `--execute` applies. Per
//! [[orca-must-never-bring-down-host]] the daemon self-updates are applied
//! SEQUENTIALLY, one host at a time, health-gated between hosts (poll the peer
//! back onto the new version before moving on), and the LOCAL host is updated
//! LAST so the controller doesn't restart itself mid-fan-out. A host that fails
//! or times out is recorded and the fan-out CONTINUES to the next host.
//!
//! CLI surface: the CLI tree turns every tool into `orca <domain> <verb>` and a
//! domain node always requires a verb (see `dispatch::cli::build_root`), so a
//! bare `orca update` is not expressible without special-casing the dispatcher.
//! This ships as `domain="update", verb="all"` → `orca update all`.

use std::time::Duration;

use anyhow::Result;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use system::commands::{SystemUpdateArgs, SystemUpdateResult};
use system::plugin_manager::{
    PluginListArgs, PluginLoadStatus, PluginUpdateArgs, PluginUpdateOutput,
};

/// Max wall-clock to wait for a peer to come back on the new version after an
/// apply before we give up on the health-gate and move to the next host.
const HEALTH_GATE_TIMEOUT: Duration = Duration::from_secs(180);
/// Poll interval while waiting for a peer to restart onto the new version.
const HEALTH_GATE_POLL: Duration = Duration::from_secs(5);
/// Sentinel peer reference for the local daemon: `server_pod::exec` treats it as
/// a loopback round-trip through the same allowlist path a peer would use.
const LOCAL_PEER: &str = "local";

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FleetUpdateArgs {
    /// Actually apply updates. DRY RUN by default: a bare `orca update all`
    /// prints the full plan (every peer's daemon current→latest, then every
    /// installed plugin installed→newest) WITHOUT changing anything. Pass
    /// `--execute` to apply.
    #[arg(long)]
    pub execute: bool,
    /// Reserved forward-compat knob for excluding known-edge/unreachable peers.
    /// There is no reliable per-peer reachability signal on the roster today, so
    /// the fan-out always attempts every joined peer and records a per-host
    /// connect error for any that don't answer. Accepted for stability.
    #[arg(long)]
    pub include_edge: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetSystemResult {
    /// Display hostname of the host.
    pub host: String,
    /// Peer id (empty for the local host).
    pub peer_id: String,
    /// Current daemon version probed on the host.
    pub current: Option<String>,
    /// Channel-latest the host would move to.
    pub target: Option<String>,
    /// True when `target` is strictly newer than `current`.
    pub update_available: bool,
    /// Version applied (execute only); `None` on a dry run or no-op.
    pub applied: Option<String>,
    /// Per-host error (probe/apply/health-gate). The fan-out continues past it.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetPluginResult {
    /// Display hostname of the host.
    pub host: String,
    /// Plugin / target-software name.
    pub name: String,
    /// Installed version on the host, or `None` when not installed.
    pub installed: Option<String>,
    /// Newest version resolved for the plugin.
    pub target: Option<String>,
    /// True when `target` is strictly newer than `installed`.
    pub update_available: bool,
    /// True when this plugin was actually (re)installed (execute only).
    pub updated: bool,
    /// Per-plugin error. The fan-out continues past it.
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetUpdateOutput {
    /// True when nothing was applied (no `--execute`).
    pub dry_run: bool,
    /// Per-host daemon update plan/results, in apply order (local last).
    pub systems: Vec<FleetSystemResult>,
    /// Per-host, per-plugin update plan/results.
    pub plugins: Vec<FleetPluginResult>,
    /// Human-readable progress/summary notes.
    pub notes: Vec<String>,
    /// Fan-out-level errors (peer enumeration, etc.), distinct from per-host ones.
    pub errors: Vec<String>,
}

/// A host to fan out to: its display name and the reference to pass to
/// `exec_remote` (peer id for a remote peer, `LOCAL_PEER` for this host).
struct Target {
    host: String,
    peer_id: String,
    peer_ref: String,
    is_local: bool,
}

/// Enumerate the joined pod peers (not departed) plus the local host, ordered
/// with the LOCAL host LAST so a self-restart never orphans the fan-out.
fn fleet_targets() -> Result<Vec<Target>> {
    let peers = db::pool::with_pooled_or_open(db::pod::list_peers)?;
    let remote: Vec<(String, String)> = peers
        .into_iter()
        .filter(|p| p.departed_at.is_none())
        .map(|p| (p.peer_hostname, p.peer_id))
        .collect();
    Ok(order_targets(
        remote,
        system::host_identity::cli_hostname_or_fallback(),
    ))
}

/// Pure ordering: joined remote peers first, the LOCAL host appended LAST so a
/// self-restart during an apply never orphans the in-flight fan-out.
fn order_targets(remote: Vec<(String, String)>, local_host: String) -> Vec<Target> {
    let mut targets: Vec<Target> = remote
        .into_iter()
        .map(|(host, peer_id)| Target {
            host,
            peer_ref: peer_id.clone(),
            peer_id,
            is_local: false,
        })
        .collect();
    targets.push(Target {
        host: local_host,
        peer_id: String::new(),
        peer_ref: LOCAL_PEER.to_string(),
        is_local: true,
    });
    targets
}

/// Strip a leading `v` so tag/version comparisons line up.
fn norm(v: &str) -> &str {
    v.trim_start_matches('v')
}

/// Poll a peer until it reports the new version (or `!update_available`), or the
/// health-gate times out. Transient errors (peer restarting) are retried.
async fn health_gate(peer_id: &str, target: &str) -> std::result::Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        // Give the peer a moment to restart before the first probe.
        tokio::time::sleep(HEALTH_GATE_POLL).await;
        match crate::peer_info::peer_update(peer_id, true).await {
            Ok(f) => {
                let on_target = f
                    .version
                    .as_deref()
                    .map(|v| norm(v) == norm(target))
                    .unwrap_or(false);
                if on_target || !f.update_available {
                    return Ok(());
                }
            }
            Err(_) => { /* peer transiently unreachable during restart — retry */ }
        }
        if start.elapsed() > HEALTH_GATE_TIMEOUT {
            return Err(format!(
                "health-gate timed out after {}s waiting for {target}",
                HEALTH_GATE_TIMEOUT.as_secs()
            ));
        }
    }
}

/// Probe (or apply, when `execute`) the daemon update on one host.
async fn run_system(t: &Target, execute: bool, ctx: &contract::ToolCtx) -> FleetSystemResult {
    let mut row = FleetSystemResult {
        host: t.host.clone(),
        peer_id: t.peer_id.clone(),
        ..Default::default()
    };
    let args = SystemUpdateArgs {
        execute,
        ..Default::default()
    };
    match dispatch::cli::exec_remote::<system::commands::SystemUpdate>(&t.peer_ref, args, ctx).await
    {
        Ok(SystemUpdateResult::Update(out)) => {
            row.current = Some(out.current_version.clone());
            row.target = out.latest.clone();
            row.update_available = out.update_available.unwrap_or(false);
            row.applied = out.applied.clone();
            if !out.errors.is_empty() {
                row.error = Some(out.errors.join("; "));
            }
        }
        Ok(_) => row.error = Some("unexpected non-update system.update result".into()),
        Err(e) => row.error = Some(format!("{e:#}")),
    }
    row
}

/// Enumerate installed plugins on one host and probe/apply each to newest.
async fn run_plugins(
    t: &Target,
    execute: bool,
    ctx: &contract::ToolCtx,
    out: &mut FleetUpdateOutput,
) {
    let list = match dispatch::cli::exec_remote::<system::plugin_manager::PluginList>(
        &t.peer_ref,
        PluginListArgs::default(),
        ctx,
    )
    .await
    {
        Ok(l) => l,
        Err(e) => {
            out.errors
                .push(format!("{}: plugin list failed: {e:#}", t.host));
            return;
        }
    };

    // Only rows that are actually present on the host are updatable.
    let installed: Vec<_> = list
        .plugins
        .into_iter()
        .filter(|p| {
            matches!(
                p.status,
                PluginLoadStatus::Loaded | PluginLoadStatus::InstalledNotLoaded
            )
        })
        .collect();

    for p in installed {
        let args = PluginUpdateArgs {
            name: p.name.clone(),
            execute,
            version: None,
            prerelease: false,
        };
        let mut row = FleetPluginResult {
            host: t.host.clone(),
            name: p.name.clone(),
            installed: p.installed_version.clone(),
            ..Default::default()
        };
        match dispatch::cli::exec_remote::<system::plugin_manager::PluginUpdate>(
            &t.peer_ref,
            args,
            ctx,
        )
        .await
        {
            Ok(PluginUpdateOutput {
                installed_version,
                target_version,
                update_available,
                executed,
                ..
            }) => {
                row.installed = installed_version;
                row.target = Some(target_version);
                row.update_available = update_available;
                row.updated = executed && update_available;
            }
            Err(e) => row.error = Some(format!("{e:#}")),
        }
        out.plugins.push(row);
    }
}

/// [MUTATES STATE] Update the WHOLE fleet. DRY RUN by default: prints the plan
/// for every joined peer's daemon then every installed plugin on every host.
/// `--execute` applies — daemons first (SEQUENTIAL, health-gated between hosts,
/// local host LAST), then all plugins everywhere. A failing host is recorded and
/// the fan-out continues. Per [[orca-must-never-bring-down-host]] the fleet is
/// never updated concurrently.
#[orca_tool(domain = "update", verb = "all", role = "admin")]
async fn update_all(args: FleetUpdateArgs, ctx: &contract::ToolCtx) -> Result<FleetUpdateOutput> {
    let _ = args.include_edge; // reserved — see FleetUpdateArgs docs
    let mut out = FleetUpdateOutput {
        dry_run: !args.execute,
        ..Default::default()
    };

    let targets = match fleet_targets() {
        Ok(t) => t,
        Err(e) => {
            out.errors.push(format!("enumerate fleet peers: {e:#}"));
            return Ok(out);
        }
    };

    // ── PHASE 1: daemons — SEQUENTIAL, health-gated, local last. ─────────────
    for t in &targets {
        let row = run_system(t, args.execute, ctx).await;
        // Health-gate only a REMOTE apply that actually landed a new binary: wait
        // for the peer back on the new version before touching the next host.
        // The local host is applied last and restarts US, so we can't gate it.
        if args.execute
            && !t.is_local
            && row.error.is_none()
            && let Some(applied) = row.applied.clone()
        {
            out.notes
                .push(format!("{}: applied {applied}, health-gating…", t.host));
            if let Err(e) = health_gate(&t.peer_id, &applied).await {
                out.notes.push(format!("{}: {e}", t.host));
            } else {
                out.notes.push(format!("{}: healthy on {applied}", t.host));
            }
        }
        out.systems.push(row);
    }

    // ── PHASE 2: plugins — every installed plugin on every host. ─────────────
    for t in &targets {
        run_plugins(t, args.execute, ctx, &mut out).await;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_strips_leading_v() {
        assert_eq!(norm("v0.1.9"), "0.1.9");
        assert_eq!(norm("0.1.9"), "0.1.9");
    }

    #[test]
    fn order_targets_puts_local_last() {
        let remote = vec![
            ("thor".to_string(), "id-thor".to_string()),
            ("loki".to_string(), "id-loki".to_string()),
        ];
        let targets = order_targets(remote, "mint".to_string());
        // Every remote peer precedes the single local entry.
        assert_eq!(targets.len(), 3);
        assert!(targets[..targets.len() - 1].iter().all(|t| !t.is_local));
        let last = targets.last().unwrap();
        assert!(last.is_local);
        assert_eq!(last.host, "mint");
        assert_eq!(last.peer_ref, LOCAL_PEER);
        assert!(last.peer_id.is_empty());
        // Remote entries dispatch by peer_id.
        assert_eq!(targets[0].peer_ref, "id-thor");
        assert!(!targets[0].is_local);
    }

    #[test]
    fn order_targets_local_only_fleet() {
        let targets = order_targets(vec![], "solo".to_string());
        assert_eq!(targets.len(), 1);
        assert!(targets[0].is_local);
    }
}
