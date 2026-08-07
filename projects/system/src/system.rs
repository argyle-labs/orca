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
/// through the pod roster (`PodPeerDto`/`PodInstance`) so parent-inference,
/// cluster grouping, and host-card rendering work without fetching the fat
/// `SystemInfoReport`. Every field here is one the roster/topology consumers
/// actually read — the heavy host facts (hardware, processes, interfaces,
/// history, charts) live on `system.info.detail` and are never dialed on a
/// read path. Projected from a collected `SystemInfoReport` via `From`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default)]
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
    /// Inferred parent peer id (mac-match on claims). Written by the pod
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
#[serde(rename_all = "snake_case")]
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

/// Which lean, read-only facet `system.detail` reports. All four are cheap
/// (local DB / on-disk state only) and never dial. The fat host snapshot stays
/// on `system.info.detail`; the paginated log/metric streams stay on
/// `system.logs` / `system.history` — none of those fold in here.
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
    /// Lean host liveness/health probe. Peer-dispatchable — the reframed
    /// replacement for the removed `pod.ping` ("ping applies to a host, not
    /// the pod"). Cheap: local daemon state only, no fan-out.
    Health,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemDetailArgs {
    /// Which facet to report. Defaults to `summary`.
    #[arg(long, value_enum, default_value = "summary")]
    #[serde(default)]
    pub view: SystemDetailView,
}

/// Lean host liveness/health probe returned by `system.detail{view=health}`.
/// Cheap enough to answer at memory speed on any host, and peer-dispatchable so
/// a controller can probe a remote host's health over the mesh.
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
}

/// Untagged so the default `view=summary` serializes as a bare
/// `SystemStatusReport` — preserving every existing wire decoder (the pod
/// `peer_detail` cache decodes `SystemStatusReport` straight from a `{}` call).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SystemDetailOutput {
    Summary(Box<SystemStatusReport>),
    Capabilities(CapabilityListOutput),
    Retention(RetentionListOutput),
    Health(HealthReport),
}

/// Snapshot of orca's installation and state. `view=summary` (default) reports
/// the lean install/state/config snapshot: binary, ~/.claude/CLAUDE.md, vault
/// dir, agents symlink, PKI init, MCP registration, plus runtime/daemon state
/// and lean topology facts. `view=capabilities` / `view=retention` surface the
/// host capability registry and retention policy; `view=health` is a lean,
/// peer-dispatchable liveness probe. The fat host snapshot (hardware/process/
/// interfaces/history) and SVG chart projections live on `system.info.detail`;
/// the paginated log/metric streams stay on `system.logs` / `system.history`.
#[orca_tool(domain = "system", verb = "detail")]
async fn system_detail(
    args: SystemDetailArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemDetailOutput> {
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
        SystemDetailView::Health => Ok(SystemDetailOutput::Health(collect_health(ctx)?)),
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

/// [MUTATES STATE when `--path`+`--owner` given] The single web-route ownership
/// tool. Read-only (omit args) it reports every registered path, its active
/// owner, and any contenders. With `--path`+`--owner` it makes that provider the
/// active owner of that exact path and persists the choice; a bad selection is
/// refused non-fatally (the incumbent keeps serving). Mirrors how a contested
/// `/` is resolved: the user picks a different UI plugin here.
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
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-tools-system-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
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

    #[tokio::test]
    async fn system_detail_health_view_is_lean() {
        let ctx = empty_ctx();
        let out = system_detail(
            SystemDetailArgs {
                view: SystemDetailView::Health,
            },
            &ctx,
        )
        .await;
        assert!(out.is_ok(), "health view failed: {:?}", out.err());
        match out.unwrap() {
            SystemDetailOutput::Health(h) => {
                assert!(!h.version.is_empty());
                assert!(h.checked_at_ms > 0);
            }
            _ => panic!("expected health view"),
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
        assert_eq!(json["db_size_bytes"], 1);
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
        assert_eq!(json["last_retention_sweep_at"], 1700000000_i64);
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
