//! Fleet-wide update fan-out — the engine behind `system.update --scope fleet`.
//!
//! A single operator action that updates the whole pod: every joined peer's
//! daemon first (PHASE 1), then every installed plugin on every host (PHASE 2).
//!
//! DRY RUN by default — it only probes current→latest per host/plugin and
//! reports whether an update is available. `execute` applies. Per
//! [[orca-must-never-bring-down-host]] the daemon self-updates are applied
//! SEQUENTIALLY, one host at a time, health-gated between hosts (poll the peer
//! back onto the new version before moving on), and the LOCAL host is updated
//! LAST so the controller doesn't restart itself mid-fan-out. A host that fails
//! or times out is recorded and the fan-out CONTINUES to the next host.
//!
//! NO tool is declared here. There is exactly ONE update operation —
//! `system.update` — and this fan-out reaches it through
//! [`system::fleet::FleetUpdateHook`], registered by the server (which is the
//! only crate that depends on both `pod` and `system`). The ergonomic bare
//! `orca update` is a CLI-ONLY alias for `system update --scope fleet`; it
//! mints no tool, endpoint, or OpenAPI tag of its own.

use std::time::Duration;

use anyhow::Result;

use system::commands::{SystemUpdateArgs, SystemUpdateResult, SystemUpdateScope};
// Fleet result types live in `system` so the hook signature is expressible
// there without `system` depending on `pod`.
pub use system::fleet::{FleetPluginResult, FleetSystemResult, FleetUpdateOutput};
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

/// Effective prerelease resolution for the plugin phase: the explicit
/// `--prerelease` flag OR the daemon's channel being Beta. A beta host resolves
/// prereleases without the flag; a stable host stays stable unless asked (#450).
fn resolve_prerelease(flag: bool, channel: system::update_state::Channel) -> bool {
    flag || channel == system::update_state::Channel::Beta
}

/// A host to fan out to: its display name and the reference to pass to
/// `exec_remote` (peer id for a remote peer, `LOCAL_PEER` for this host).
pub(crate) struct Target {
    pub(crate) host: String,
    pub(crate) peer_id: String,
    pub(crate) peer_ref: String,
    pub(crate) is_local: bool,
}

/// Enumerate the joined pod peers (not departed) plus the local host, ordered
/// with the LOCAL host LAST so a self-restart never orphans the fan-out.
pub(crate) fn fleet_targets() -> Result<Vec<Target>> {
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

/// Dispatch a tool at one target. The LOCAL host runs it IN-PROCESS via the
/// tool's own `OrcaTool::run` (ctx carries no `--peer`, so no mesh round-trip):
/// a host updating itself must never go through `pod/exec` peer verification and
/// fail on "no pinned bootstrap key" for its own identity (#451). Remote peers
/// dispatch over the mesh as before.
pub(crate) async fn dispatch_at<T: contract::OrcaTool>(
    t: &Target,
    args: T::Args,
    ctx: &contract::ToolCtx,
) -> Result<T::Output> {
    if t.is_local {
        <T as contract::OrcaTool>::run(args, ctx).await
    } else {
        dispatch::cli::exec_remote::<T>(&t.peer_ref, args, ctx).await
    }
}

/// Poll a peer until it reports the new version (or `!update_available`), or the
/// health-gate times out. Transient errors (peer restarting) are retried.
async fn health_gate(peer_id: &str, target: &str) -> std::result::Result<(), String> {
    let start = std::time::Instant::now();
    // Kept so a timeout can say WHY it never converged. Reporting a bare
    // "timed out" leaves the operator with 180s of silence and no cause.
    let mut last_err: Option<String> = None;
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
            Err(e) => {
                let msg = e.to_string();
                // A restart makes the peer unreachable for a few seconds, which is
                // the whole reason this loop exists — but an error retrying cannot
                // fix (the verb is gone from the peer's build, the credential was
                // refused) must abort NOW. Swallowing it burnt the full 180s on
                // all 7 hosts of the rc.2 -> rc.3 roll, turning the between-host
                // safety check into a sleep ([[orca-must-never-bring-down-host]]).
                if utils::probe_error::classify(&msg) == utils::probe_error::ProbeOutcome::Permanent
                {
                    return Err(format!(
                        "health-gate cannot verify {target}: {msg} (unretryable — \
                         not waiting out the {}s gate)",
                        HEALTH_GATE_TIMEOUT.as_secs()
                    ));
                }
                last_err = Some(msg);
            }
        }
        if start.elapsed() > HEALTH_GATE_TIMEOUT {
            return Err(match last_err {
                Some(e) => format!(
                    "health-gate timed out after {}s waiting for {target}; last probe error: {e}",
                    HEALTH_GATE_TIMEOUT.as_secs()
                ),
                None => format!(
                    "health-gate timed out after {}s waiting for {target}; peer answered but \
                     never reported {target}",
                    HEALTH_GATE_TIMEOUT.as_secs()
                ),
            });
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
    // Pin HOST scope explicitly: the per-host leg of the fan-out must never
    // re-enter the fleet fan-out, whatever the default scope becomes.
    let args = SystemUpdateArgs {
        execute,
        scope: Some(SystemUpdateScope::Host),
        ..Default::default()
    };
    match dispatch_at::<system::commands::SystemUpdate>(t, args, ctx).await {
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
    prerelease: bool,
    ctx: &contract::ToolCtx,
    out: &mut FleetUpdateOutput,
) {
    let list =
        match dispatch_at::<system::plugin_manager::PluginList>(t, PluginListArgs::default(), ctx)
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
            prerelease,
        };
        let mut row = FleetPluginResult {
            host: t.host.clone(),
            name: p.name.clone(),
            installed: p.installed_version.clone(),
            ..Default::default()
        };
        match dispatch_at::<system::plugin_manager::PluginUpdate>(t, args, ctx).await {
            Ok(PluginUpdateOutput {
                installed_version,
                target_version,
                update_available,
                executed,
                note,
                ..
            }) => {
                row.installed = installed_version;
                row.target = Some(target_version);
                row.update_available = update_available;
                row.updated = executed && update_available;
                row.note = Some(note);
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
///
/// Plain function, NOT an `#[orca_tool]`: reached only through
/// [`system::fleet::FleetUpdateHook`] from the one `system.update` tool.
pub async fn fleet_update(
    execute: bool,
    prerelease: bool,
    ctx: &contract::ToolCtx,
) -> Result<FleetUpdateOutput> {
    let mut out = FleetUpdateOutput {
        dry_run: !execute,
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
        let row = run_system(t, execute, ctx).await;
        // Health-gate only a REMOTE apply that actually landed a new binary: wait
        // for the peer back on the new version before touching the next host.
        // The local host is applied last and restarts US, so we can't gate it.
        if execute
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
    // Resolve prerelease once: explicit flag OR this daemon's channel is beta.
    let prerelease = resolve_prerelease(
        prerelease,
        system::update_state::read_channel_marker()
            .unwrap_or(system::update_state::Channel::Stable),
    );
    for t in &targets {
        run_plugins(t, execute, prerelease, ctx, &mut out).await;
    }

    Ok(out)
}

/// Pod's implementation of the `system` fleet-update seam. Registered on the
/// `ToolCtx` by the server (see `server::mcp::build_tool_ctx`), alongside
/// `ServerHostRefreshHook`.
pub struct PodFleetUpdateHook;

#[async_trait::async_trait]
impl system::fleet::FleetUpdateHook for PodFleetUpdateHook {
    async fn fleet_update(
        &self,
        execute: bool,
        prerelease: bool,
        ctx: &contract::ToolCtx,
    ) -> Result<FleetUpdateOutput> {
        fleet_update(execute, prerelease, ctx).await
    }
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
    fn resolve_prerelease_flag_and_channel() {
        use system::update_state::Channel;
        // Explicit flag always wins.
        assert!(resolve_prerelease(true, Channel::Stable));
        assert!(resolve_prerelease(true, Channel::Beta));
        // Unset defaults to the daemon channel: beta ⇒ prerelease, stable ⇒ not.
        assert!(resolve_prerelease(false, Channel::Beta));
        assert!(!resolve_prerelease(false, Channel::Stable));
    }

    #[tokio::test]
    async fn dispatch_at_local_runs_in_process_never_pod_exec() {
        use contract::{OrcaTool, OrcaToolDef};
        use schemars::JsonSchema;
        use serde::{Deserialize, Serialize};
        use std::sync::Arc;

        #[derive(Serialize, Deserialize, JsonSchema)]
        struct A;
        #[derive(Serialize, Deserialize, JsonSchema, PartialEq, Debug)]
        struct Out {
            ran_local: bool,
        }
        struct Probe;
        impl OrcaToolDef for Probe {
            type Args = A;
            type Output = Out;
            const NAME: &'static str = "test.probe";
            const DESCRIPTION: &'static str = "probe";
            const REMOTE_OK: bool = true;
        }
        #[async_trait::async_trait]
        impl OrcaTool for Probe {
            async fn run(_args: A, _ctx: &contract::ToolCtx) -> Result<Out> {
                Ok(Out { ran_local: true })
            }
        }

        let cfg = Arc::new(contract::config::Config::load().unwrap());
        // No RemoteExec service registered: the mesh path would error, so a
        // successful call proves the local target ran in-process (#451).
        let ctx = contract::ToolCtx::new(cfg);

        let local = Target {
            host: "self".into(),
            peer_id: String::new(),
            peer_ref: LOCAL_PEER.to_string(),
            is_local: true,
        };
        let out = dispatch_at::<Probe>(&local, A, &ctx).await.unwrap();
        assert_eq!(out, Out { ran_local: true });

        // A remote target with no RemoteExec service registered errors — the
        // local target above must NOT have taken this path.
        let remote = Target {
            host: "peer".into(),
            peer_id: "id-peer".into(),
            peer_ref: "id-peer".into(),
            is_local: false,
        };
        assert!(dispatch_at::<Probe>(&remote, A, &ctx).await.is_err());
    }

    #[test]
    fn order_targets_local_only_fleet() {
        let targets = order_targets(vec![], "solo".to_string());
        assert_eq!(targets.len(), 1);
        assert!(targets[0].is_local);
    }

    #[test]
    fn declares_no_empty_domain_update_tool() {
        // The phantom top-level `update` DOMAIN is gone: this module declares no
        // tool at all, so nothing registers with an empty domain. `orca update`
        // survives only as the CLI alias for `system update --scope fleet`.
        assert!(
            dispatch::cli::ops().all(|o| !o.domain.is_empty()),
            "no op may register with an empty domain"
        );
    }

    #[test]
    fn bare_orca_update_is_a_cli_alias_defaulting_to_fleet_scope() {
        let root = dispatch::cli::build_root(clap::Command::new("orca"));
        // `orca update` still parses, and defaults `--scope fleet`.
        let m = root
            .clone()
            .try_get_matches_from(["orca", "update"])
            .expect("`orca update` must parse");
        let (name, sub) = m.subcommand().expect("update subcommand present");
        assert_eq!(name, "update");
        assert_eq!(
            sub.get_one::<system::commands::SystemUpdateScope>("scope"),
            Some(&system::commands::SystemUpdateScope::Fleet)
        );
        // `--execute` (and the other fleet args) still parse on the alias.
        let m = root
            .try_get_matches_from(["orca", "update", "--execute", "--prerelease"])
            .expect("`orca update --execute` must parse");
        let (_, sub) = m.subcommand().unwrap();
        assert!(sub.get_flag("execute"));
        assert!(sub.get_flag("prerelease"));
        // The alias resolves back to the ONE `system.update` op.
        assert_eq!(
            dispatch::cli::alias_target("update"),
            Some(("system", "update"))
        );
    }

    #[test]
    fn hook_forwards_to_the_fan_out() {
        // The seam is the only entry point; assert it is wired to the one
        // fan-out function (type-level — running it would need a peer roster).
        fn assert_hook<T: system::fleet::FleetUpdateHook>() {}
        assert_hook::<PodFleetUpdateHook>();
    }
}
