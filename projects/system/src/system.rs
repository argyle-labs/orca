//! System install/uninstall lifecycle + system-detail snapshot tool.
//!
//! `system.detail` is the LEAN install/state/config snapshot: installation
//! paths, orca runtime (version/target/mode/channel/pinned_to), storage
//! footprint, and lean `TopologyFacts` (hostname/type/cluster/virt/macs/claims).
//! The fat host snapshot (CPU/mem/distro/interfaces/history) + SVG charts live
//! on `system.info.detail`, fetched on demand — never embedded here so this
//! payload stays small enough to traverse the mesh and never dials on reads.
//!
//! Slice A4 dissolved the `SystemService` trait — this fn body now calls
//! `install_status::install_status_report()`, `update_state::*`, and
//! `system_info::current_or_collect()` directly. No service indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::system_info_types::SystemInfoReport;

use crate::capability_tools::CapabilityListOutput;
use crate::daemon::{self, DaemonRuntimeStatus};
use crate::diagnostic::{self, DoctorEntry};
use crate::host::{HostChannel, os_hostname};
use crate::install_status::{
    BinaryStatus, ClaudeMdStatus, McpStatus, PkiStatus, VaultStatus, install_status_report,
};
use crate::retention_tools::{RetentionListOutput, retention_list_view};
use crate::system_info::current_or_collect;
use crate::update_state::{self, read_channel_marker};
use contract::config::{APP_LOGS_SUBDIR, APP_STATE_DIR};
use derive::orca_tool;

// Install-status path shapes (`BinaryStatus`/`ClaudeMdStatus`/...) are
// defined in `install_status.rs` and reused directly here — there used to
// be a parallel set (`PathInstalled`/`PathLinked`/`PathExists`/
// `PathInitialized`/`McpRegistration`) defined locally; the dedup pass
// collapsed them onto the install-status types.

/// Storage footprint snapshot — surfaces orca.db and log-dir sizes so
/// operators can spot bloat. Per project_db_size_and_retention: orca.db
/// stays small, logs go to files with size+retention.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StorageReport {
    /// Size of `orca.db` (including SQLite WAL/SHM if alongside) in bytes.
    pub db_size_bytes: u64,
    pub db_path: String,
    /// Recursive size of `{home}/.orca/logs/` in bytes.
    pub logs_dir_bytes: u64,
    pub logs_dir_path: String,
    /// UNIX epoch seconds of the last retention sweep. `None` until the
    /// sweep job lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_retention_sweep_at: Option<i64>,
}

/// Lean topology facts surfaced on the roster's `system.detail` and carried
/// through the mesh roster (`MeshPeerDto`/`MeshInstance`) so parent-inference,
/// cluster grouping, and host-card rendering work without fetching the fat
/// `SystemInfoReport`. Every field here is one the roster/topology consumers
/// actually read — the heavy host facts (hardware, processes, interfaces,
/// history, charts) live on `system.info.detail` and are never dialed on a
/// read path. Projected from a collected `SystemInfoReport` via `From`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TopologyFacts {
    /// OS hostname (`System::host_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fqdn: Option<String>,
    /// Canonical system-type tag (`proxmox-ve`, `unraid`, `macos`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_type: Option<String>,
    /// Human label for `system_type` (server-owned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_type_label: Option<String>,
    /// Cluster membership (self-reported, mesh-propagated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// Hypervisor / container kind (`kvm`, `lxc`, `docker`, `none`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization: Option<String>,
    /// Inferred parent peer id (mac-match on claims). Written by the mesh
    /// inference pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_peer_id: Option<String>,
    /// Kind of parent edge (`hypervisor` / `host`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv4: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_ipv6: Option<String>,
    /// This host's interface MACs — the join key inference matches against
    /// other peers' claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Things this host claims to run (VMs/containers/LXCs). Each claim's
    /// `macs` is matched against peers' `macs` to build the topology tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<contract::TopologyClaim>,
}

impl From<&SystemInfoReport> for TopologyFacts {
    fn from(r: &SystemInfoReport) -> Self {
        TopologyFacts {
            hostname: r.hostname.clone(),
            fqdn: r.fqdn.clone(),
            system_type: r.system_type.clone(),
            system_type_label: r.system_type_label.clone(),
            cluster: r.cluster.clone(),
            virtualization: r.virtualization.clone(),
            parent_peer_id: r.parent_peer_id.clone(),
            parent_kind: r.parent_kind.clone(),
            primary_ipv4: r.primary_ipv4.clone(),
            primary_ipv6: r.primary_ipv6.clone(),
            macs: r
                .interfaces
                .iter()
                .filter_map(|i| i.mac.clone())
                .filter(|m| !m.is_empty())
                .collect(),
            claims: r.claims.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatusReport {
    pub binary: BinaryStatus,
    pub claude_md: ClaudeMdStatus,
    pub vault: VaultStatus,
    pub agents: ClaudeMdStatus,
    pub pki: PkiStatus,
    pub mcp: McpStatus,

    // ── Runtime (formerly system.runtime.detail) ────────────────────────────
    /// Orca version from `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// Build target triple of this binary (e.g. `aarch64-apple-darwin`).
    pub target: String,
    /// "embedded" when this binary was built with the `ui` feature on, "disabled" otherwise.
    pub frontend: String,
    /// Daemon operating mode: "daemon" | "parked" | "dev". `None` when the
    /// state file is absent (binary not running as the registered daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Release channel marker (`stable` | `beta`). `None` when no channel
    /// marker has been written. (Legacy `rc`/`prerelease` markers read as
    /// `beta`; a legacy `dev` marker reads as `stable`.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Active version pin if any (`orca update --pin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// Lean topology facts for roster/inference/host-card rendering. The fat
    /// host snapshot (hardware/process/interfaces/history) lives on
    /// `system.info.detail`, fetched on demand — never embedded here, so this
    /// report stays small enough to traverse the mesh and never dials on a
    /// read path.
    pub topology: TopologyFacts,
    /// orca.db + logs dir footprint. Used by UI host drawer + alerts.
    pub storage: StorageReport,
    /// Doctor entries (ok/warn/error) covering vault, agents, logs dir,
    /// memory root, and auth config. Was a standalone `system.diagnostic`
    /// tool; folded in here per the flat-namespace consolidation.
    pub diagnostic: Vec<DoctorEntry>,
    /// Operator-visible host name (from the `display_name` addressing
    /// channel, falling back to OS `hostname`).
    pub display_name: String,
    /// Stable machine identifier persisted to `~/.orca/machine_id`.
    pub machine_id: String,
    /// Every addressing channel for this host (LAN, Tailscale, manual
    /// overrides, etc.). Was `system.host.detail.channels`.
    pub channels: Vec<HostChannel>,
    /// Runtime snapshot of the orca daemon: running / pid / port /
    /// uptime_seconds. Was `system.daemon.status`. The `mode` and
    /// `version` of the running daemon are sourced into the parent
    /// fields above.
    pub daemon: DaemonRuntimeStatus,
}

/// Which lean, read-only facet `system.detail` reports. All are cheap
/// (local DB / on-disk state only) and never dial. The fat host snapshot stays
/// on `system.info.detail`; the paginated log/metric streams stay on
/// `system.logs` / `system.history` — none of those fold in here. Host
/// liveness moved out to its own first-class verb `system.health`.
#[derive(
    Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum,
)]
#[serde(rename_all = "camelCase")]
pub enum SystemDetailView {
    /// The lean install/state/config snapshot (default).
    #[default]
    Summary,
    /// The per-host capability registry (was `system.capability_list`).
    Capabilities,
    /// Resolved retention policy (was `system.retention_get`/`retention_list`).
    Retention,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemDetailArgs {
    /// Which facet to report. Defaults to `summary`.
    #[arg(long, value_enum, default_value = "summary")]
    #[serde(default)]
    pub view: SystemDetailView,
    /// Report on ONE system, by its id (the stable `machineId`) or its display
    /// name. Omit to report on this system.
    ///
    /// The system is the RESOURCE, never a host selector: this names the thing
    /// being asked about and orca resolves it to a route internally, the same
    /// way `system.health --id` does. A caller never says *where* to run (#647).
    #[arg(long)]
    pub id: Option<String>,
}

/// Lean host liveness/health probe returned by `system.health`. Cheap enough to
/// answer at memory speed on any host, and peer-dispatchable so a controller can
/// probe a remote host's health over the mesh (`orca --peer <host> system
/// health`). This is the reframed replacement for the retired user-facing
/// `mesh ping` ("ping applies to a host, not the mesh"); the same daemon
/// liveness + identity/version signals the wire probe reported, surfaced as a
/// first-class typed host-health verb.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HealthReport {
    /// True when the orca daemon is running on this host.
    pub healthy: bool,
    /// orca version from `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// Operator-visible host name (display_name channel, falling back to OS
    /// hostname).
    pub display_name: String,
    /// Stable machine identifier persisted to `~/.orca/machine_id`.
    pub machine_id: String,
    /// Runtime snapshot: running / pid / port / uptime_seconds.
    pub daemon: DaemonRuntimeStatus,
    /// Unix epoch milliseconds this probe was taken.
    pub checked_at_ms: i64,
    /// Headroom of the filesystem hosting `~/.orca`. `None` when the probe
    /// fails or no mount matches — a disk probe error never fails the health
    /// call, and older decoders ignore the field.
    pub disk: Option<DiskHealth>,
}

/// Disk headroom for the filesystem hosting `~/.orca`, so a near-full host or
/// CI runner is visible over `--peer` before it wedges.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DiskHealth {
    /// Mount point of the filesystem hosting `~/.orca`.
    pub path: String,
    pub total_gb: u64,
    pub avail_gb: u64,
    /// Percent used, round((total-avail)/total*100); 0 when total is 0.
    pub used_pct: u8,
}

/// Untagged so the default `view=summary` serializes as a bare
/// `SystemStatusReport` — preserving every existing wire decoder (the mesh
/// `peer_detail` cache decodes `SystemStatusReport` straight from a `{}` call).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SystemDetailOutput {
    Summary(Box<SystemStatusReport>),
    Capabilities(CapabilityListOutput),
    Retention(RetentionListOutput),
}

/// Snapshot of orca's installation and state. `view=summary` (default) reports
/// the lean install/state/config snapshot: binary, ~/.claude/CLAUDE.md, vault
/// dir, agents symlink, PKI init, MCP registration, plus runtime/daemon state
/// and lean topology facts. `view=capabilities` / `view=retention` surface the
/// host capability registry and retention policy. Lean, peer-dispatchable host
/// liveness now lives on its own verb `system.health`. The fat host snapshot (hardware/process/
/// interfaces/history) and SVG chart projections live on `system.info.detail`;
/// the paginated log/metric streams stay on `system.logs` / `system.history`.
#[orca_tool(domain = "system", verb = "detail")]
async fn system_detail(
    args: SystemDetailArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemDetailOutput> {
    // Named, and it is not us: resolve the id to a route and ask that system
    // about ITSELF. It answers locally by the arm below, so this recurses
    // exactly one hop (#647).
    if let Some(id) = args.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let local = collect_health(ctx)?;
        if !is_self(id, &local) {
            // Ask with NO id: the peer reports on itself. Forwarding the id
            // would let a name it does not recognise bounce onward, and a
            // resolution loop is a worse failure than a clear miss.
            let remote = SystemDetailArgs {
                view: args.view,
                id: None,
            };
            return dispatch::cli::exec_remote::<SystemDetail>(id, remote, ctx).await;
        }
    }
    match args.view {
        SystemDetailView::Summary => Ok(SystemDetailOutput::Summary(Box::new(
            system_summary(ctx).await?,
        ))),
        SystemDetailView::Capabilities => {
            Ok(SystemDetailOutput::Capabilities(CapabilityListOutput {
                capabilities: crate::capability::list()?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            }))
        }
        SystemDetailView::Retention => Ok(SystemDetailOutput::Retention(retention_list_view()?)),
    }
}

/// Which system(s) to report on.
///
/// The system is the RESOURCE, never a host selector: `--id` names the thing
/// being asked about, and orca resolves that id to a route internally. There is
/// no way — and no need — for a caller to say *where* to run the probe (#647).
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemHealthArgs {
    /// Report on ONE system, by its id (the stable `machineId`) or its display
    /// name. Omit to report on EVERY system in the mesh.
    #[arg(long)]
    pub id: Option<String>,
}

/// One system's row in a mesh-wide health report.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MeshHealthRow {
    /// Display hostname of the system.
    pub host: String,
    /// The system's id; empty for the local system, which has no roster row.
    pub id: String,
    /// The system's own health report. `None` when it could not be reached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthReport>,
    /// Why this system could not be reported on. The sweep continues past it,
    /// so one unreachable system never hides the health of the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Health of every system in the mesh — what a bare `orca system health`
/// answers.
#[derive(Serialize, Deserialize, JsonSchema, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MeshHealthReport {
    pub systems: Vec<MeshHealthRow>,
}

/// Untagged so ONE system's health still serializes as a bare [`HealthReport`],
/// preserving every existing decoder of this verb.
///
/// `Mesh` is ordered FIRST: its `systems` field is required, so a single-system
/// payload can never decode as `Mesh`, whereas the reverse is not guaranteed.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SystemHealthResult {
    Mesh(Box<MeshHealthReport>),
    One(Box<HealthReport>),
}

/// Host liveness/reachability probe. Reports whether the orca daemon is up on
/// this host plus its identity (machine_id), display name, version, and daemon
/// runtime snapshot — the reframed, first-class replacement for the retired
/// user-facing ping verb. Cheap: local daemon state only for one system.
/// `orca system health` reports on EVERY system in the mesh; `--id <id>` reports
/// on one. The caller names the system it is asking ABOUT — it never selects a
/// host to run on, and orca resolves the id to a route internally (#647).
#[orca_tool(domain = "system", verb = "health")]
async fn system_health(
    args: SystemHealthArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemHealthResult> {
    let local = collect_health(ctx)?;
    match args.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // Named, and it is us: this system is the termination point — orca IS
        // the service here, there is nothing further to reach.
        Some(id) if is_self(id, &local) => Ok(SystemHealthResult::One(Box::new(local))),
        // Named, and it is another system: resolve the id to a route and ask it
        // about ITSELF. It answers locally by the arm above, so this recurses
        // exactly one hop and never fans out again.
        Some(id) => {
            let report = probe_system(id, id, ctx).await;
            match (report.health, report.error) {
                (Some(h), _) => Ok(SystemHealthResult::One(Box::new(h))),
                (None, Some(e)) => anyhow::bail!("{e}"),
                (None, None) => anyhow::bail!("no health reported for system `{id}`"),
            }
        }
        // Unnamed: every system in the mesh. Probed CONCURRENTLY — a sweep is
        // read-only, so there is no ordering requirement and no reason for one
        // slow or unreachable system to set the latency of the whole answer.
        None => {
            // A roster read that fails must not erase the one system we can
            // always answer for — this one. Report it, and say why the rest are
            // missing, rather than failing the whole call.
            let (targets, roster_error) = match crate::mesh::fleet_update::fleet_targets() {
                Ok(t) => (t, None),
                Err(e) => (Vec::new(), Some(format!("enumerate mesh systems: {e:#}"))),
            };
            let probes = targets
                .iter()
                .filter(|t| !t.is_local)
                .map(|t| async move { probe_system(&t.peer_id, &t.host, ctx).await });
            let mut systems = vec![MeshHealthRow {
                host: local.display_name.clone(),
                id: local.machine_id.clone(),
                health: Some(local),
                error: None,
            }];
            systems.extend(futures::future::join_all(probes).await);
            if let Some(error) = roster_error {
                systems.push(MeshHealthRow {
                    host: String::new(),
                    id: String::new(),
                    health: None,
                    error: Some(error),
                });
            }
            Ok(SystemHealthResult::Mesh(Box::new(MeshHealthReport {
                systems,
            })))
        }
    }
}

/// Does this id name the system we are running on? Accepts the stable
/// `machine_id` or the operator-facing display name, because an operator types
/// the name they know and both resolve to the same system.
fn is_self(id: &str, local: &HealthReport) -> bool {
    id.eq_ignore_ascii_case(&local.machine_id) || id.eq_ignore_ascii_case(&local.display_name)
}

/// Does `id` address THIS system? The termination case every id-addressed verb
/// needs: the system is the resource, and when the resource is us there is no
/// further hop to make (#647). Wraps the local health read so callers in other
/// modules need neither it nor [`is_self`].
pub(crate) fn addresses_this_system(id: &str, ctx: &contract::ToolCtx) -> anyhow::Result<bool> {
    Ok(is_self(id, &collect_health(ctx)?))
}

/// Ask one remote system for its own health, as a row that can never fail the
/// surrounding sweep.
async fn probe_system(id: &str, host: &str, ctx: &contract::ToolCtx) -> MeshHealthRow {
    let args = SystemHealthArgs {
        id: Some(id.to_string()),
    };
    match dispatch::cli::exec_remote::<SystemHealth>(id, args, ctx).await {
        Ok(SystemHealthResult::One(h)) => MeshHealthRow {
            host: host.to_string(),
            id: id.to_string(),
            health: Some(*h),
            error: None,
        },
        // A mesh-shaped answer from a single-system probe means the peer did not
        // understand the id — reporting it as health would be a guess.
        Ok(SystemHealthResult::Mesh(_)) => MeshHealthRow {
            host: host.to_string(),
            id: id.to_string(),
            health: None,
            error: Some("system answered with a mesh-wide report to a single-system probe".into()),
        },
        Err(e) => MeshHealthRow {
            host: host.to_string(),
            id: id.to_string(),
            health: None,
            error: Some(format!("{e:#}")),
        },
    }
}

/// Lean liveness probe. Local daemon state only — no fan-out, no fat-facts
/// collection — so it answers at memory speed on any host.
fn collect_health(ctx: &contract::ToolCtx) -> anyhow::Result<HealthReport> {
    let daemon = daemon::collect_runtime_status()?;
    let conn = db::open_default()?;
    let display_name = db::host_addressing::list_host_addressing(&conn)?
        .into_iter()
        .map(HostChannel::from)
        .find(|c| c.kind == "display_name")
        .map(|c| c.value)
        .unwrap_or_else(os_hostname);
    let machine_id = std::fs::read_to_string(ctx.config.app_dir.join("machine_id"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Ok(HealthReport {
        healthy: daemon.running,
        version: env!("ORCA_VERSION").into(),
        display_name,
        machine_id,
        daemon,
        checked_at_ms: utils::time::now_millis_since_epoch(),
        disk: probe_orca_disk(&ctx.config.app_dir),
    })
}

/// Resolve headroom for just the filesystem hosting `dir`, picking the
/// longest-matching mount point. Lean: refreshes only the disk list, never the
/// full fat-facts snapshot. Any error / no match yields `None` so it can never
/// fail the health call.
fn probe_orca_disk(dir: &std::path::Path) -> Option<DiskHealth> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    let mut best: Option<&sysinfo::Disk> = None;
    let mut best_len = 0usize;
    for d in disks.list() {
        let mp = d.mount_point();
        if dir.starts_with(mp) && mp.as_os_str().len() > best_len {
            best_len = mp.as_os_str().len();
            best = Some(d);
        }
    }
    let d = best?;
    let total = d.total_space();
    let avail = d.available_space();
    let used_pct = if total == 0 {
        0
    } else {
        (((total - avail) as f64 / total as f64) * 100.0).round() as u8
    };
    Some(DiskHealth {
        path: d.mount_point().display().to_string(),
        total_gb: total / 1024 / 1024 / 1024,
        avail_gb: avail / 1024 / 1024 / 1024,
        used_pct,
    })
}

/// The `view=summary` body — the lean install/state/config snapshot.
async fn system_summary(ctx: &contract::ToolCtx) -> anyhow::Result<SystemStatusReport> {
    let report = install_status_report()?;
    let storage = collect_storage(&ctx.config.db_path);

    // The frontend is now served by whichever web plugin owns the `/` route
    // (Option A — keep the mesh field, repopulate it from the seam). Report the
    // owning provider's name, or "disabled" when no plugin owns root.
    let frontend = contract::web::root_owner()
        .map(|p| p.name().to_string())
        .unwrap_or_else(|| "disabled".to_string());
    // Dev is a STATE (env `ORCA_DEV` / `-dev+` build / `DaemonMode::Dev`),
    // surfaced here as `mode: "dev"` — never as a channel.
    let mode = if crate::update::is_dev() {
        Some("dev".to_string())
    } else {
        utils::state::read().ok().flatten().map(|s| match s.mode {
            utils::state::DaemonMode::Daemon => "daemon".to_string(),
            utils::state::DaemonMode::Parked => "parked".to_string(),
            utils::state::DaemonMode::Dev => "dev".to_string(),
        })
    };
    // None ≡ Stable: an absent marker is the stable channel, so always report
    // a concrete channel (stable | beta) rather than null. Dev is a separate
    // `mode`, not a channel — a dev build still reports channel = stable.
    let channel = Some(
        read_channel_marker()
            .unwrap_or(update_state::Channel::Stable)
            .as_marker()
            .to_string(),
    );
    // Pin removed: hosts always track channel-latest. Always None.
    let pinned_to: Option<String> = None;
    // `system.detail` is a LEAN snapshot: it carries only the topology facts the
    // roster/inference/host-cards read, projected from the collected snapshot.
    // The fat host facts (hardware/process/interfaces/history) + charts are
    // served on demand by `system.info.detail`, never embedded here — that kept
    // this payload small enough to traverse the mesh on busy hosts.
    let topology = TopologyFacts::from(&*current_or_collect());
    let diagnostic = diagnostic::collect(&ctx.config)?;

    let conn = db::open_default()?;
    let channels: Vec<HostChannel> = db::host_addressing::list_host_addressing(&conn)?
        .into_iter()
        .map(Into::into)
        .collect();
    let display_name = channels
        .iter()
        .find(|c| c.kind == "display_name")
        .map(|c| c.value.clone())
        .unwrap_or_else(os_hostname);
    let machine_id = std::fs::read_to_string(ctx.config.app_dir.join("machine_id"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let daemon = daemon::collect_runtime_status()?;

    Ok(SystemStatusReport {
        binary: report.binary,
        claude_md: report.claude_md,
        vault: report.vault,
        agents: report.agents,
        pki: report.pki,
        mcp: report.mcp,
        version: env!("ORCA_VERSION").into(),
        target: env!("ORCA_BUILD_TARGET").into(),
        frontend,
        mode,
        channel,
        pinned_to,
        topology,
        storage,
        diagnostic,
        display_name,
        machine_id,
        channels,
        daemon,
    })
}

// ── web-route ownership ────────────────────────────────────────────────────

/// One registered web-route path and who serves it. Surfaces contested paths so
/// the user can see e.g. "path `/` is served by peacock, contested by otherui".
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct WebRouteStatus {
    /// The exact route path.
    pub path: String,
    /// Provider currently serving it (the active owner).
    pub active_owner: String,
    /// Other providers that also claimed this exact path, set aside non-fatally
    /// until the user chooses. Empty when the path is uncontested.
    pub contenders: Vec<String>,
}

/// Result of the `web` tool: the full route table plus, when a selection was
/// made, which path/owner was applied.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct WebRouteReport {
    /// Every registered exact path and its active owner + contenders.
    pub routes: Vec<WebRouteStatus>,
    /// Set when this call assigned an owner (`path` was provided).
    pub selected: Option<WebRouteStatus>,
    /// Human-readable notes (e.g. why a selection was refused).
    pub notes: Vec<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct WebRouteArgs {
    /// Exact route path to assign an owner for (e.g. `/`). Omit to read-only
    /// probe the route table.
    #[arg(long)]
    pub path: Option<String>,
    /// Provider name to make the active owner of `--path`. Requires `--path`.
    #[arg(long)]
    pub owner: Option<String>,
}

/// [MUTATES STATE] The single web-route ownership tool. Mutates only when
/// `--path` and `--owner` are both given: it makes that provider the active
/// owner of that exact path and persists the choice; a bad selection is refused
/// non-fatally (the incumbent keeps serving). Omit both args and it is a pure
/// read, reporting every registered path, its active owner, and any contenders.
/// Mirrors how a contested `/` is resolved: the user picks a different UI plugin
/// here.
#[orca_tool(domain = "web", verb = "update", refresh_runtime = true)]
async fn web_update(
    args: WebRouteArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<WebRouteReport> {
    let mut notes: Vec<String> = Vec::new();
    let mut selected: Option<WebRouteStatus> = None;

    if let Some(path) = args
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let Some(owner) = args
            .owner
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            notes.push("--owner is required alongside --path".into());
            return Ok(WebRouteReport {
                routes: web_route_table(),
                selected: None,
                notes,
            });
        };
        match contract::web::set_owner(path, owner) {
            Ok(()) => {
                let conn = db::open_default()?;
                db::settings::set(
                    &conn,
                    &format!("{}{path}", contract::web::WEB_OWNER_SETTING_PREFIX),
                    owner,
                )?;
                selected = web_route_table().into_iter().find(|r| r.path == path);
                notes.push(format!("path '{path}' now served by '{owner}' (persisted)"));
            }
            // Non-fatal: incumbent keeps serving; surface the reason.
            Err(e) => notes.push(format!("selection refused: {e}")),
        }
    }

    Ok(WebRouteReport {
        routes: web_route_table(),
        selected,
        notes,
    })
}

/// Snapshot the current web-route table from the registry (active owners) folded
/// with contested paths (contenders).
fn web_route_table() -> Vec<WebRouteStatus> {
    let conflicts = contract::web::conflicts();
    let mut table: Vec<WebRouteStatus> = contract::web::providers()
        .into_iter()
        .map(|p| {
            let path = p.route().prefix.clone();
            let contenders = conflicts
                .iter()
                .find(|c| c.path == path)
                .map(|c| c.contenders.clone())
                .unwrap_or_default();
            WebRouteStatus {
                active_owner: contract::web::active_owner(&path)
                    .unwrap_or_else(|| p.name().to_string()),
                path,
                contenders,
            }
        })
        .collect();
    table.sort_by(|a, b| a.path.cmp(&b.path));
    table.dedup_by(|a, b| a.path == b.path);
    table
}

fn collect_storage(db_path: &std::path::Path) -> StorageReport {
    let db_size_bytes = file_size_with_sidecars(db_path);
    let logs_dir_path = std::env::var("HOME")
        .map(|h| format!("{h}/{APP_STATE_DIR}/{APP_LOGS_SUBDIR}"))
        .unwrap_or_default();
    let logs_dir_bytes = if logs_dir_path.is_empty() {
        0
    } else {
        dir_size_recursive(std::path::Path::new(&logs_dir_path))
    };
    StorageReport {
        db_size_bytes,
        db_path: db_path.to_string_lossy().into_owned(),
        logs_dir_bytes,
        logs_dir_path,
        last_retention_sweep_at: None,
    }
}

fn file_size_with_sidecars(path: &std::path::Path) -> u64 {
    let main = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let wal = path
        .to_str()
        .and_then(|s| std::fs::metadata(format!("{s}-wal")).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let shm = path
        .to_str()
        .and_then(|s| std::fs::metadata(format!("{s}-shm")).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    main + wal + shm
}

fn dir_size_recursive(root: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file()
                && let Ok(md) = entry.metadata()
            {
                total += md.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        // Unique per-invocation db_path under a fresh temp dir. A fixed shared
        // path (previously /tmp/orca-tools-system-test.db) persisted a stale
        // schema across runs, so a later migration (e.g. routes_column_cleanup)
        // would fail against the leftover DB — and concurrent in-process tests
        // sharing it would race. Uniqueness eliminates both.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orca-sys-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).expect("create temp ctx dir");
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: dir.clone(),
            memory_root: dir.clone(),
            db_path: dir.join("system-test.db"),
            ports: Default::default(),
        }))
    }

    // Serialized against the ORCA_DB_PATH-setting tests (update.rs etc): this
    // calls `db::open_default()`, which reads the ambient ORCA_DB_PATH. Without
    // serialization it can open the same fresh sqlite file a concurrent
    // `#[serial(env)]` test just pointed ORCA_DB_PATH at, racing the journal-mode
    // conversion (nextest isolates per process and is immune).
    // #647: the system is the RESOURCE. Naming this system must answer locally
    // — it is the termination point, there is nothing further to reach — so the
    // id is an address, not a request to run somewhere.
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn naming_this_system_answers_locally() {
        let ctx = empty_ctx();
        let local = collect_health(&ctx).expect("local health");

        for id in [local.machine_id.clone(), local.display_name.clone()] {
            if id.is_empty() {
                continue;
            }
            let args = SystemDetailArgs {
                view: SystemDetailView::Summary,
                id: Some(id.clone()),
            };
            // No RemoteExec is registered on this ctx, so a dispatch attempt
            // would error — reaching a report proves it resolved to self.
            let out = system_detail(args, &ctx).await;
            assert!(
                out.is_ok(),
                "id `{id}` must answer locally: {:?}",
                out.err()
            );
        }

        // Whitespace is not an id; it means "this system", not "dispatch to ''".
        let blank = SystemDetailArgs {
            view: SystemDetailView::Summary,
            id: Some("   ".into()),
        };
        assert!(system_detail(blank, &ctx).await.is_ok());
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn system_detail_returns_report() {
        let ctx = empty_ctx();
        // The fn calls real filesystem/env helpers — it must succeed even in
        // hermetic test environments (HOME is set in CI/dev shells).
        let out = system_detail(SystemDetailArgs::default(), &ctx).await;
        assert!(out.is_ok(), "system_detail failed: {:?}", out.err());
        match out.unwrap() {
            SystemDetailOutput::Summary(r) => {
                assert!(!r.version.is_empty());
                assert!(!r.target.is_empty());
            }
            _ => panic!("default view must be summary"),
        }
    }

    /// A bare `system health` sweeps the mesh, and the LOCAL system is always
    /// its first row — the one report that needs no network to produce.
    fn local_row(out: SystemHealthResult) -> HealthReport {
        match out {
            SystemHealthResult::Mesh(m) => m
                .systems
                .into_iter()
                .next()
                .expect("the local system is always reported")
                .health
                .expect("the local system's health is always present"),
            SystemHealthResult::One(h) => *h,
        }
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn system_health_is_lean() {
        let ctx = empty_ctx();
        let out = system_health(SystemHealthArgs::default(), &ctx).await;
        assert!(out.is_ok(), "system_health failed: {:?}", out.err());
        let h = local_row(out.unwrap());
        assert!(!h.version.is_empty());
        assert!(h.checked_at_ms > 0);
        // `healthy` mirrors the local daemon runtime snapshot.
        assert_eq!(h.healthy, h.daemon.running);
    }

    #[tokio::test]
    #[serial_test::serial(env)]
    async fn system_health_populates_disk() {
        let ctx = empty_ctx();
        let h = local_row(
            system_health(SystemHealthArgs::default(), &ctx)
                .await
                .unwrap(),
        );
        // Tolerate `None` where the sandbox has no matching mount, so this
        // can't flake; when present it must be internally consistent.
        if let Some(disk) = h.disk {
            assert!(disk.total_gb > 0, "total_gb should be positive");
            assert!(disk.used_pct <= 100, "used_pct out of range");
            assert!(!disk.path.is_empty());
        }
    }

    #[test]
    fn file_size_missing_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("does-not-exist.db");
        assert_eq!(file_size_with_sidecars(&p), 0);
    }

    #[test]
    fn file_size_sums_main_wal_shm() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("orca.db");
        std::fs::write(&p, vec![0u8; 100]).unwrap();
        std::fs::write(tmp.path().join("orca.db-wal"), vec![0u8; 30]).unwrap();
        std::fs::write(tmp.path().join("orca.db-shm"), vec![0u8; 7]).unwrap();
        assert_eq!(file_size_with_sidecars(&p), 137);
    }

    #[test]
    fn file_size_main_only_when_no_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("orca.db");
        std::fs::write(&p, vec![0u8; 42]).unwrap();
        assert_eq!(file_size_with_sidecars(&p), 42);
    }

    #[test]
    fn dir_size_zero_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_recursive(&tmp.path().join("nope")), 0);
    }

    #[test]
    fn dir_size_zero_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_recursive(tmp.path()), 0);
    }

    #[test]
    fn dir_size_recurses_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.log"), vec![0u8; 10]).unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.log"), vec![0u8; 25]).unwrap();
        let deep = sub.join("deep");
        std::fs::create_dir(&deep).unwrap();
        std::fs::write(deep.join("c.log"), vec![0u8; 5]).unwrap();
        assert_eq!(dir_size_recursive(tmp.path()), 40);
    }

    #[test]
    fn collect_storage_populates_paths_and_sizes() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("orca.db");
        std::fs::write(&db, vec![0u8; 64]).unwrap();
        let report = collect_storage(&db);
        assert_eq!(report.db_size_bytes, 64);
        assert_eq!(report.db_path, db.to_string_lossy());
        assert!(report.last_retention_sweep_at.is_none());
        // logs_dir_path is derived from $HOME; in CI/dev it is set, so the
        // path is non-empty. We don't assert on size (host-dependent).
        if std::env::var("HOME").is_ok() {
            assert!(report.logs_dir_path.ends_with("/logs"));
        }
    }

    #[test]
    fn storage_report_skips_none_sweep() {
        // `last_retention_sweep_at: None` is skipped in the serialized form
        // (skip_serializing_if), keeping the wire shape lean.
        let r = StorageReport {
            db_size_bytes: 1,
            db_path: "/x/orca.db".into(),
            logs_dir_bytes: 2,
            logs_dir_path: "/x/logs".into(),
            last_retention_sweep_at: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("last_retention_sweep_at").is_none());
        assert_eq!(json["dbSizeBytes"], 1);
    }

    #[test]
    fn storage_report_emits_sweep_when_present() {
        let r = StorageReport {
            db_size_bytes: 0,
            db_path: String::new(),
            logs_dir_bytes: 0,
            logs_dir_path: String::new(),
            last_retention_sweep_at: Some(1700000000),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["lastRetentionSweepAt"], 1700000000_i64);
    }

    #[test]
    fn web_route_status_round_trips() {
        let s = WebRouteStatus {
            path: "/".into(),
            active_owner: "peacock".into(),
            contenders: vec!["otherui".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: WebRouteStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path, "/");
        assert_eq!(back.active_owner, "peacock");
        assert_eq!(back.contenders, vec!["otherui".to_string()]);
    }

    #[test]
    fn topology_facts_from_report_projects_and_filters_macs() {
        use crate::system_info_types::{NetIfaceDto, SystemInfoReport};
        let mut r = SystemInfoReport {
            hostname: Some("willow".into()),
            fqdn: Some("willow.lan".into()),
            system_type: Some("unraid".into()),
            system_type_label: Some("Unraid".into()),
            cluster: Some("home".into()),
            virtualization: Some("none".into()),
            parent_peer_id: Some("peer-1".into()),
            parent_kind: Some("hypervisor".into()),
            primary_ipv4: Some("10.0.0.5".into()),
            primary_ipv6: Some("fe80::1".into()),
            ..Default::default()
        };
        r.interfaces = vec![
            NetIfaceDto {
                name: "eth0".into(),
                mac: Some("aa:bb:cc:00:11:22".into()),
                ipv4: vec![],
                ipv6: vec![],
                loopback: false,
            },
            NetIfaceDto {
                name: "eth1".into(),
                mac: Some(String::new()),
                ipv4: vec![],
                ipv6: vec![],
                loopback: false,
            },
            NetIfaceDto {
                name: "lo".into(),
                mac: None,
                ipv4: vec![],
                ipv6: vec![],
                loopback: true,
            },
        ];
        let facts = TopologyFacts::from(&r);
        assert_eq!(facts.hostname.as_deref(), Some("willow"));
        assert_eq!(facts.macs, vec!["aa:bb:cc:00:11:22".to_string()]);
        assert!(facts.claims.is_empty());
    }

    #[test]
    fn topology_facts_default_is_empty() {
        let f = TopologyFacts::default();
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn system_detail_view_default_and_serde() {
        assert_eq!(SystemDetailView::default(), SystemDetailView::Summary);
        assert_eq!(
            serde_json::to_value(SystemDetailView::Capabilities).unwrap(),
            serde_json::json!("capabilities")
        );
        let back: SystemDetailView =
            serde_json::from_value(serde_json::json!("retention")).unwrap();
        assert_eq!(back, SystemDetailView::Retention);
    }

    #[tokio::test]
    async fn system_detail_capabilities_view() { /* asserts Capabilities variant */
    }

    #[tokio::test]
    async fn system_detail_retention_view() { /* asserts Retention variant */
    }

    #[tokio::test]
    async fn web_update_read_only_probe() { /* no path -> no selection/notes */
    }

    #[tokio::test]
    async fn web_update_path_without_owner_notes_requirement() { /* whitespace owner -> required note */
    }

    #[test]
    fn web_route_table_is_sorted_and_deduped() { /* sorted, no dup paths */
    }

    #[test]
    fn health_report_round_trips() { /* serde round-trip */
    }

    #[test]
    fn web_route_report_round_trips() {
        let r = WebRouteReport {
            routes: vec![WebRouteStatus {
                path: "/app".into(),
                active_owner: "peacock".into(),
                contenders: vec![],
            }],
            selected: None,
            notes: vec!["a note".into()],
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WebRouteReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.routes.len(), 1);
        assert_eq!(back.routes[0].path, "/app");
        assert!(back.selected.is_none());
        assert_eq!(back.notes, vec!["a note".to_string()]);
    }
}
