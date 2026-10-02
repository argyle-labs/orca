//! Fleet-wide update fan-out — the engine behind `system.update` with no `--id`.
//!
//! A single operator action that updates the whole mesh: every joined peer's
//! daemon first (PHASE 1), then every installed plugin on every host (PHASE 2).
//!
//! DRY RUN by default — it only probes current→latest per host/plugin and
//! reports whether an update is available. `execute` applies. Per
//! [[orca-must-never-bring-down-host]] the daemon self-updates are applied
//! SEQUENTIALLY, one host at a time, health-gated between hosts (poll the peer
//! back onto the new version before moving on), and the LOCAL host is updated
//! LAST — after the plugin phase, not merely last within the daemon phase — so
//! the controller doesn't restart itself mid-fan-out. That distinction is the
//! whole of #649: "last" used to mean "last in phase 1", and the local apply's
//! 2-second detached restart then landed partway through a plugin phase that
//! iterates every host. A host that fails or times out is recorded and the
//! fan-out CONTINUES to the next host.
//!
//! NO tool is declared here. There is exactly ONE update operation —
//! `system.update` — and this fan-out reaches it through
//! [`crate::fleet::FleetUpdateHook`], registered by the server (which is the
//! only crate that depends on both `mesh` and `system`). The ergonomic bare
//! `orca update` is a CLI-ONLY alias for `system update`; it
//! mints no tool, endpoint, or OpenAPI tag of its own.

use std::time::Duration;

use anyhow::{Context, Result};
use derive::orca_tool;

use crate::commands::{SystemUpdateArgs, SystemUpdateResult};
// Fleet result types live in `system` so the hook signature is expressible
// there without `system` depending on `mesh`.
pub use crate::fleet::{FleetPluginResult, FleetSystemResult, FleetUpdateOutput};
use crate::plugin_manager::{
    PluginListArgs, PluginLoadStatus, PluginUpdateArgs, PluginUpdateOutput,
};

/// Max wall-clock to wait for a peer to come back on the new version after an
/// apply before we give up on the health-gate and move to the next host.
const HEALTH_GATE_TIMEOUT: Duration = Duration::from_secs(180);
/// Poll interval while waiting for a peer to restart onto the new version.
const HEALTH_GATE_POLL: Duration = Duration::from_secs(5);
/// Sentinel peer reference for the local daemon: `exec::exec` treats it as
/// a loopback round-trip through the same allowlist path a peer would use.
const LOCAL_PEER: &str = "local";
/// This controller's running version. Reported by the gate-skew note (#623) and
/// used for the readiness probe's cert status, which takes its version from the
/// caller rather than reading a crate-local `CARGO_PKG_VERSION`.
const CONTROLLER_VERSION: &str = env!("ORCA_VERSION");

/// Where a fan-out writes its run record. One file per run, under the orca
/// home's `logs/`. `None` when no home is resolvable (nothing to persist to).
fn run_record_path(started: &utils::time::Timestamp) -> Option<std::path::PathBuf> {
    let dir = contract::config::paths::orca_home()?.join(contract::config::APP_LOGS_SUBDIR);
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(run_record_filename(started)))
}

/// Run-record filename for a start instant. Pure, so the naming is testable
/// without resolving — or creating — an orca home.
fn run_record_filename(started: &utils::time::Timestamp) -> String {
    // Colons are legal on the filesystems we target but awkward in shells.
    format!(
        "fleet-update-{}.json",
        started.to_rfc3339().replace(':', "-")
    )
}

/// Write the report so far. Called after EVERY host, and in particular BEFORE
/// the local apply: updating the local host restarts this daemon and severs the
/// caller's connection, so anything only held in memory is lost (#625). The
/// record is the durable copy — best-effort, and a failure here never fails the
/// roll.
fn persist(out: &FleetUpdateOutput, path: Option<&std::path::Path>) {
    let Some(path) = path else { return };
    match serde_json::to_vec_pretty(out) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(path, bytes) {
                tracing::warn!(path = %path.display(), error = %e, "fleet-update run record not written");
            }
        }
        Err(e) => tracing::warn!(error = %e, "fleet-update run record not serializable"),
    }
}

/// How long a fleet lock stays valid without its holder finishing. A roll is
/// bounded by the health gate per host, so this is sized well above a worst-case
/// full-fleet roll — long enough that a slow run is never broken into, short
/// enough that a controller killed mid-roll does not block the fleet forever.
const FLEET_LOCK_TTL: Duration = Duration::from_secs(90 * 60);

/// Where the fleet lock lives. The controller's orca home, NOT shared storage:
/// the lock's job is to stop two rolls being driven at once, and every roll is
/// driven from a controller. A second controller is a real gap, named in #616
/// and deliberately not solved here — solving it needs fleet-shared state, and
/// the measured incident was two sessions on ONE controller.
fn fleet_lock_path() -> Option<std::path::PathBuf> {
    Some(contract::config::paths::orca_home()?.join("fleet-update.lock"))
}

/// Holder of the fleet lock; releases on drop so a panicking or early-returning
/// roll cannot strand it.
struct FleetLock {
    path: std::path::PathBuf,
}

impl Drop for FleetLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %e, "fleet lock not released");
        }
    }
}

/// Is a lock started at `started` stale as of `now`? Pure so the TTL boundary is
/// testable without touching a filesystem or waiting 90 minutes.
///
/// An unparseable or future-dated timestamp counts as NOT stale: refusing a
/// second roll is the safe failure, and the operator has `break_lock`.
fn lock_is_stale(started: &str, now: &utils::time::Timestamp) -> bool {
    let Ok(started) = utils::time::Timestamp::parse_rfc3339(started) else {
        return false;
    };
    now.unix_seconds().saturating_sub(started.unix_seconds()) > FLEET_LOCK_TTL.as_secs() as i64
}

/// What a lock file holds. Every field is `default`, so a lock written by an
/// older (or newer) build still reads back well enough to name a holder —
/// failing to parse the lock would make it look free, which is the one outcome
/// this file exists to prevent.
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct LockRecord {
    /// The system driving the roll.
    host: String,
    /// Its process id, so an operator can check whether it is genuinely stuck.
    pid: Option<u32>,
    /// When the roll started, RFC3339 — also what the TTL is measured from.
    started_at: String,
}

/// Human-readable refusal for a live lock — names the holder and when it
/// started, because "already running" with no holder is unactionable (#616).
fn lock_refusal(rec: &LockRecord) -> String {
    let host = if rec.host.is_empty() {
        "unknown"
    } else {
        &rec.host
    };
    let started = if rec.started_at.is_empty() {
        "unknown time"
    } else {
        &rec.started_at
    };
    let pid = rec.pid.map(|p| format!(" pid {p}")).unwrap_or_default();
    format!(
        "a fleet roll is already in flight (started by {host}{pid} at {started}) — \
         the fleet must never be updated concurrently, because each run's health gate \
         would be judging a version the other run is changing underneath it. Wait for \
         it, or pass `--break-lock` if that run is genuinely stuck."
    )
}

/// Take the fleet-wide single-flight lock, or explain why not.
///
/// `Ok(None)` means there is nowhere to persist a lock (no resolvable orca
/// home) — the roll proceeds unlocked rather than refusing to run at all.
/// `Err` is a live holder, and the caller must NOT proceed (#616).
fn acquire_fleet_lock(
    break_lock: bool,
    now: &utils::time::Timestamp,
) -> Result<Option<FleetLock>, String> {
    let Some(path) = fleet_lock_path() else {
        return Ok(None);
    };
    if let Ok(bytes) = std::fs::read(&path) {
        let rec: LockRecord = serde_json::from_slice(&bytes).unwrap_or_default();
        if !break_lock && !lock_is_stale(&rec.started_at, now) {
            return Err(lock_refusal(&rec));
        }
    }
    let rec = LockRecord {
        host: crate::host_identity::cli_hostname_or_fallback(),
        pid: Some(std::process::id()),
        started_at: now.to_rfc3339(),
    };
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        tracing::warn!(dir = %dir.display(), error = %e, "fleet lock dir not created");
    }
    match std::fs::write(&path, serde_json::to_vec_pretty(&rec).unwrap_or_default()) {
        Ok(()) => Ok(Some(FleetLock { path })),
        // A lock we cannot write is a lock we cannot honour. Say so and run:
        // blocking every roll on a read-only home would be the worse failure.
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "fleet lock not taken");
            Ok(None)
        }
    }
}

/// Effective prerelease resolution for the plugin phase: the explicit
/// `--prerelease` flag OR the daemon's channel being Beta. A beta host resolves
/// prereleases without the flag; a stable host stays stable unless asked (#450).
fn resolve_prerelease(flag: bool, channel: crate::update_state::Channel) -> bool {
    flag || channel == crate::update_state::Channel::Beta
}

/// A host to fan out to: its display name and the reference to pass to
/// `exec_remote` (peer id for a remote peer, `LOCAL_PEER` for this host).
pub(crate) struct Target {
    pub(crate) host: String,
    pub(crate) peer_id: String,
    pub(crate) peer_ref: String,
    pub(crate) is_local: bool,
}

/// Enumerate the joined mesh peers (not departed) plus the local host, ordered
/// with the LOCAL host LAST so a self-restart never orphans the fan-out.
pub(crate) fn fleet_targets() -> Result<Vec<Target>> {
    let peers = db::pool::with_pooled_or_open(db::mesh::list_peers)?;
    let remote: Vec<(String, String)> = peers
        .into_iter()
        .filter(|p| p.departed_at.is_none())
        .map(|p| (p.peer_hostname, p.peer_id))
        .collect();
    Ok(order_targets(
        remote,
        crate::host_identity::cli_hostname_or_fallback(),
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
/// a host updating itself must never go through `mesh/exec` peer verification and
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
///
/// Probes through [`dispatch_at`] — the SAME authenticated path the apply uses —
/// deliberately. It previously went via `peer_info::peer_update` -> `exec_peer`,
/// whose signature takes no `ToolCtx`, so it had nothing to source a signed
/// caller token from and every peer that enforces `role = admin` on
/// `system.update` refused it:
///
/// ```text
/// mesh/exec refused: tool 'system.update' requires role 'admin'
/// but no signed caller token was presented
/// ```
///
/// That was not a version-skew problem. It was structural, it applied to every
/// host, and it meant this gate had never once verified a peer — the between-host
/// safety check from [[orca-must-never-bring-down-host]] was inert while looking
/// merely slow, because the refusal was retried for the full timeout.
///
/// A dry-run `system.update` (`execute: false`) is the right probe: it reports
/// the peer's current version and whether an update is still outstanding, which
/// is exactly the convergence question, and it reuses the credential the operator
/// already presented to drive the apply.
async fn health_gate(
    t: &Target,
    target: &str,
    ctx: &contract::ToolCtx,
) -> std::result::Result<(), String> {
    let start = std::time::Instant::now();
    // Kept so a timeout can say WHY it never converged. Reporting a bare
    // "timed out" leaves the operator with 180s of silence and no cause.
    let mut last_err: Option<String> = None;
    loop {
        // Give the peer a moment to restart before the first probe.
        tokio::time::sleep(HEALTH_GATE_POLL).await;
        // Dry run: probe only, never apply. `self_only` for the same reason the
        // apply pins it — a per-host leg must not re-enter the fan-out.
        let probe = SystemUpdateArgs {
            execute: false,
            self_only: true,
            ..Default::default()
        };
        match dispatch_at::<crate::commands::SystemUpdate>(t, probe, ctx).await {
            Ok(SystemUpdateResult::Update(out)) => {
                let on_target = norm(&out.current_version) == norm(target);
                if on_target || !out.update_available.unwrap_or(false) {
                    return Ok(());
                }
            }
            // A shape we don't recognise is not convergence evidence. Record it
            // and keep polling rather than reporting a peer healthy on a guess.
            Ok(_) => {
                last_err = Some("unexpected non-update system.update result".into());
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
        id: t.peer_id.clone(),
        ..Default::default()
    };
    // Pin `self_only` explicitly: the per-host leg of the fan-out must never
    // re-enter it. Now that no id means EVERY system, this pin is what keeps a
    // leg from fanning out again.
    let args = SystemUpdateArgs {
        execute,
        self_only: true,
        ..Default::default()
    };
    match dispatch_at::<crate::commands::SystemUpdate>(t, args, ctx).await {
        Ok(SystemUpdateResult::Update(out)) => {
            row.current = Some(out.current_version.clone());
            row.target = out.latest.clone();
            row.update_available = out.update_available.unwrap_or(false);
            row.applied = out.applied.clone();
            // `None` from a peer too old to compute it means UNKNOWN, which must
            // not render as "can fetch" — but it must not render as blocked
            // either. Defaulting false keeps the pre-#652 behaviour for old
            // peers and only ever adds information for new ones.
            row.blocked = out.fetch_blocked.unwrap_or(false);
            row.blocked_reason = out.fetch_blocked_reason.clone();
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
        match dispatch_at::<crate::plugin_manager::PluginList>(t, PluginListArgs::default(), ctx)
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

    // A probe APPLIES NOTHING, so the per-plugin lookups have no ordering
    // requirement and run concurrently. This was the inner loop of the cost in
    // #626: one release-source lookup per plugin per host, in series, is what
    // made a read-only fleet probe outlast an MCP client's idle timeout.
    //
    // An APPLY stays strictly sequential. Concurrent plugin installs on one
    // host race the plugin manager, and a fast wrong answer is not the trade
    // being made here.
    if execute {
        for p in installed {
            let row = probe_or_apply(
                t,
                &p.name,
                p.installed_version.clone(),
                true,
                prerelease,
                ctx,
            )
            .await;
            out.plugins.push(row);
        }
        return;
    }
    let probes = installed.iter().map(|p| {
        probe_or_apply(
            t,
            &p.name,
            p.installed_version.clone(),
            false,
            prerelease,
            ctx,
        )
    });
    out.plugins.extend(futures::future::join_all(probes).await);
}

/// Probe (or apply) one plugin on one host, yielding its result row.
///
/// Extracted so the concurrent probe path and the sequential apply path run
/// IDENTICAL per-plugin logic — two copies would be two chances for a dry run
/// and an apply to disagree about what they observed.
async fn probe_or_apply(
    t: &Target,
    name: &str,
    installed: Option<String>,
    execute: bool,
    prerelease: bool,
    ctx: &contract::ToolCtx,
) -> FleetPluginResult {
    let args = PluginUpdateArgs {
        name: name.to_string(),
        execute,
        version: None,
        prerelease,
    };
    let mut row = FleetPluginResult {
        host: t.host.clone(),
        name: name.to_string(),
        installed,
        ..Default::default()
    };
    match dispatch_at::<crate::plugin_manager::PluginUpdate>(t, args, ctx).await {
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
    row
}

/// [MUTATES STATE] Update the WHOLE fleet. DRY RUN by default: reports the plan
/// for every joined peer's daemon then every installed plugin on every host.
/// `--execute` applies.
///
/// ## Order, and why
///
/// Per [[orca-must-never-bring-down-host]] the fleet is never updated
/// concurrently and the controller never restarts itself mid-fan-out. Three
/// phases deliver that:
///
/// 1. **Remote daemons** — sequential, health-gated between hosts.
/// 2. **Plugins** — every installed plugin on every host, including this one.
/// 3. **The LOCAL daemon** — genuinely last.
///
/// Phase 3 used to sit at the end of phase 1, which made "local last" true only
/// *within* phase 1. Applying locally schedules a 2-second detached supervisor
/// restart (sized for a host-scope update, where the RPC returns immediately),
/// so SIGTERM landed a couple of seconds into a plugin phase that iterates every
/// host — and which plugins actually got updated was a race decided by how far
/// phase 2 had walked (#649). Moving the local apply behind phase 2 means the
/// one step that kills this process is the last work there is.
///
/// A failing host is recorded and the fan-out continues past it.
///
/// Plain function, NOT an `#[orca_tool]`: reached only through
/// [`crate::fleet::FleetUpdateHook`] from the one `system.update` tool.
pub async fn fleet_update(
    req: crate::fleet::FleetUpdateRequest,
    ctx: &contract::ToolCtx,
) -> Result<FleetUpdateOutput> {
    let crate::fleet::FleetUpdateRequest {
        execute,
        prerelease,
        break_lock,
        daemons_only,
    } = req;
    let started = utils::time::now();
    let mut out = FleetUpdateOutput {
        dry_run: !execute,
        ..Default::default()
    };

    // Chosen before any work so the path can be reported even if the run dies.
    let record = run_record_path(&started);
    if let Some(p) = record.as_deref() {
        out.run_record = Some(p.display().to_string());
        out.notes.push(format!("run record: {}", p.display()));
    }

    // Single-flight, for an APPLY only: a read-only probe changes nothing, so it
    // must neither take the lock nor be blocked by one (#616). Held until the
    // function returns — including the local apply, whose SIGTERM drops it.
    let _lock = if execute {
        match acquire_fleet_lock(break_lock, &started) {
            Ok(l) => l,
            Err(refusal) => {
                out.errors.push(refusal);
                persist(&out, record.as_deref());
                return Ok(out);
            }
        }
    } else {
        None
    };

    let targets = match fleet_targets() {
        Ok(t) => t,
        Err(e) => {
            out.errors.push(format!("enumerate fleet peers: {e:#}"));
            persist(&out, record.as_deref());
            return Ok(out);
        }
    };
    // Refuse UP FRONT when this controller cannot dial at all, rather than
    // emitting one blamed-peer row per host for a purely local fault (#618).
    if targets.iter().any(|t| !t.is_local)
        && let Some(refusal) =
            controller_dial_readiness(&crate::mesh::pki_dir(), CONTROLLER_VERSION)
    {
        out.errors.push(refusal);
        persist(&out, record.as_deref());
        return Ok(out);
    }
    persist(&out, record.as_deref());

    // Resolve prerelease once: explicit flag OR this daemon's channel is beta.
    let prerelease = resolve_prerelease(
        prerelease,
        crate::update_state::read_channel_marker().unwrap_or(crate::update_state::Channel::Stable),
    );

    // A dry run applies nothing, so it has no ordering requirement and no reason
    // to be slow. Probing every host concurrently is what makes a fleet-wide
    // without `--execute` — the natural way to ask "what version is everything
    // on" — usable at all: serially it ran long enough for MCP clients to abort
    // it at their 300s idle timeout (#626).
    if !execute {
        let probes = targets.iter().map(|t| async move {
            let row = run_system(t, false, ctx).await;
            let mut plugins = FleetUpdateOutput::default();
            // `--daemons-only` answers "what version is every host on" without
            // the plugin phase at all — the dominant cost, and unrelated to
            // the question (#626).
            if !daemons_only {
                run_plugins(t, false, prerelease, ctx, &mut plugins).await;
            }
            (row, plugins)
        });
        for (row, plugins) in futures::future::join_all(probes).await {
            note_system_progress(&row);
            out.systems.push(row);
            out.plugins.extend(plugins.plugins);
            out.errors.extend(plugins.errors);
        }
        persist(&out, record.as_deref());
        return Ok(out);
    }

    // ── PHASE 1: REMOTE daemons — sequential, health-gated. ──────────────────
    for t in targets.iter().filter(|t| !t.is_local) {
        let row = run_system(t, true, ctx).await;
        // Health-gate an apply that actually landed a new binary: wait for the
        // peer back on the new version before touching the next host.
        if row.error.is_none()
            && let Some(applied) = row.applied.clone()
        {
            let note = format!("{}: applied {applied}, health-gating…", t.host);
            tracing::info!("{note}");
            out.notes.push(note);
            let note = match health_gate(t, &applied, ctx).await {
                Err(e) => format!("{}: {e}", t.host),
                Ok(()) => format!("{}: healthy on {applied}", t.host),
            };
            tracing::info!("{note}");
            out.notes.push(note);
        }
        note_system_progress(&row);
        out.systems.push(row);
        persist(&out, record.as_deref());
    }

    // Blame the right side before going further: N connect-class failures with
    // no successes is one local fault, not N target faults (#618).
    let remote_rows: Vec<&FleetSystemResult> =
        out.systems.iter().filter(|r| !r.id.is_empty()).collect();
    if let Some(diag) = all_peers_unreached(&remote_rows) {
        tracing::warn!("{diag}");
        out.errors.push(diag);
    }

    // Say plainly that a gate change in the payload was not exercised (#623).
    let applied: Vec<&str> = out
        .systems
        .iter()
        .filter_map(|r| r.applied.as_deref())
        .collect();
    if let Some(note) = gate_skew_note(CONTROLLER_VERSION, &applied) {
        tracing::info!("{note}");
        out.notes.push(note);
    }

    // ── PHASE 2: plugins — every installed plugin on every host. ─────────────
    // Skipped entirely under `--daemons-only`, which then means "roll the
    // daemons, leave plugins alone". Said in the output rather than inferred
    // from an empty list, which is indistinguishable from "no plugins".
    if daemons_only {
        out.notes.push(
            "plugin phase SKIPPED (--daemons-only): no plugin was probed or \
                   updated on any host"
                .to_string(),
        );
    } else {
        for t in &targets {
            run_plugins(t, true, prerelease, ctx, &mut out).await;
            persist(&out, record.as_deref());
        }
    }

    // ── PHASE 3: the LOCAL daemon — genuinely last. ──────────────────────────
    // This apply restarts THIS daemon ~2s later, so everything gathered so far
    // is flushed BEFORE it: after it there may be no process left to write, and
    // the caller's connection dies with the process (#625).
    for t in targets.iter().filter(|t| t.is_local) {
        let note = format!(
            "{}: applying LOCALLY last — this daemon restarts, so the caller's \
             connection drops here; the run record above is the durable copy",
            t.host
        );
        tracing::info!("{note}");
        out.notes.push(note);
        persist(&out, record.as_deref());
        let row = run_system(t, true, ctx).await;
        note_system_progress(&row);
        out.systems.push(row);
        persist(&out, record.as_deref());
    }

    Ok(out)
}

/// Does this controller hold the mesh identity needed to DIAL a peer? Returns a
/// refusal when it does not.
///
/// A roll launched ~30s after the controller's own daemon restarted no-opped the
/// entire fleet, reporting `No route to host` against all nine peers while the
/// network was provably fine (#618). `system.health` said healthy at 17s uptime,
/// because an HTTP daemon answering says nothing about whether the mesh
/// transport can open an OUTBOUND connection — liveness is not readiness.
///
/// Checked only when there are remote targets: a founder alone in its own mesh
/// updates itself perfectly well and must not be refused.
fn controller_dial_readiness(pki_dir: &std::path::Path, version: &str) -> Option<String> {
    let st = utils::pki::mesh_cert_status(pki_dir, version);
    let mut missing = Vec::new();
    if !st.member {
        missing.push("mesh CA certificate");
    }
    if st.leaf_client.is_none() {
        missing.push("mesh client certificate");
    }
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "controller is not ready to dial the mesh: {} missing from {}. Every peer would be \
         reported unreachable and the fault is LOCAL, not on the targets. If this daemon just \
         restarted, let it finish starting and retry; otherwise re-join the mesh.",
        missing.join(" and "),
        pki_dir.display()
    ))
}

/// Errors that mean "the connection never got there" — as opposed to a peer that
/// answered and refused. Used to tell a controller-side fault from N independent
/// target faults (#618).
fn is_unreached(msg: &str) -> bool {
    const NEVER_REACHED: &[&str] = &[
        "No route to host",
        "Connection refused",
        "Operation timed out",
        "connection timed out",
        "error sending request",
        "dns error",
        "Network is unreachable",
        "no route to host",
    ];
    NEVER_REACHED.iter().any(|n| msg.contains(n))
}

/// When EVERY remote host failed and every failure is connect-class, say so once
/// as a controller-side fault instead of leaving N rows that each blame a peer.
///
/// The measured output read as nine independent target failures when the single
/// actual fault was local, which points diagnosis in exactly the wrong direction
/// — the natural next step from that output is to investigate the network and
/// the peers, and both were healthy (#618).
///
/// Requires more than one host: a single unreachable peer is far likelier to be
/// that peer's own problem than the controller's.
fn all_peers_unreached(rows: &[&FleetSystemResult]) -> Option<String> {
    if rows.len() < 2 {
        return None;
    }
    if !rows
        .iter()
        .all(|r| r.error.as_deref().is_some_and(is_unreached))
    {
        return None;
    }
    Some(format!(
        "all {} remote hosts were unreachable, every one with a connect-level error. That is \
         overwhelmingly a CONTROLLER-side fault (mesh transport not ready, or this host's own \
         networking), not {} independent target failures — check this host before the peers.",
        rows.len(),
        rows.len()
    ))
}

/// Warn that the gate logic this roll SHIPS is not the gate logic it RAN.
///
/// The controller updates itself last, so it runs the old gate for the whole
/// fan-out and a fix to the health gate cannot be exercised by the release that
/// delivers it (#623). Local-last is correct and stays — it is what stops the
/// controller restarting itself mid-fan-out, per
/// [[orca-must-never-bring-down-host]]. The defect is that nobody was told, so
/// two consecutive releases shipped gate fixes that were inert.
fn gate_skew_note(controller_version: &str, applied: &[&str]) -> Option<String> {
    let newer = applied
        .iter()
        .find(|v| norm(v) != norm(controller_version))?;
    Some(format!(
        "gate-skew: this controller ran the health-gate logic of {controller_version} for the \
         whole fan-out while rolling peers to {newer} — the controller updates itself LAST, so \
         any health-gate change in {newer} was NOT exercised by this roll. Re-run once this \
         daemon is on {newer} to validate it (#623)."
    ))
}

/// Log one host's daemon outcome as it happens.
///
/// The whole fan-out buffers its result until the end, so a multi-minute roll
/// wrote one line of output for ~21 minutes and there was no way to tell work
/// from a hang (#608). These lines land in `daemon.jsonl` while the roll is
/// still running, which is the progress signal operators were reduced to
/// grepping for side effects to get.
fn note_system_progress(row: &FleetSystemResult) {
    match (&row.error, &row.applied) {
        (Some(e), _) => tracing::warn!(host = %row.host, "FAILED: {e}"),
        (None, Some(v)) => tracing::info!(host = %row.host, "applied {v}"),
        (None, None) => tracing::info!(
            host = %row.host,
            current = row.current.as_deref().unwrap_or("?"),
            target = row.target.as_deref().unwrap_or("?"),
            update_available = row.update_available,
            "probed"
        ),
    }
}

/// Mesh's implementation of the `system` fleet-update seam. Registered on the
/// `ToolCtx` by the server (see `server::mcp::build_tool_ctx`), alongside
/// `ServerHostRefreshHook`.
pub struct MeshFleetUpdateHook;

#[async_trait::async_trait]
impl crate::fleet::FleetUpdateHook for MeshFleetUpdateHook {
    async fn fleet_update(
        &self,
        req: crate::fleet::FleetUpdateRequest,
        ctx: &contract::ToolCtx,
    ) -> Result<FleetUpdateOutput> {
        fleet_update(req, ctx).await
    }
}

/// Newest fleet-update run record on this host, if any.
///
/// Pure over a directory listing so the "newest" rule is testable without
/// running a roll.
fn newest_record_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("fleet-update-") && n.ends_with(".json"))
        .collect();
    // The filename encodes an RFC3339 instant, so lexicographic order IS
    // chronological order — already pinned by
    // `run_record_filename_is_shell_safe_and_sorts_chronologically`.
    names.sort();
    names.pop().map(|n| dir.join(n))
}

#[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct FleetUpdateStatusOutput {
    /// The roll itself, exactly as the run record has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<crate::fleet::FleetUpdateOutput>,
    /// Absolute path of the record this came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_record: Option<String>,
    /// Seconds since the record was last written. A roll updates it after every
    /// host, so a small age means one is probably still running and a large one
    /// means this is the last completed roll. Stated as an age rather than
    /// "running: true/false" because a controller killed mid-roll leaves a
    /// record that nothing will ever finish, and claiming it is live would be a
    /// guess.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_age_secs: Option<u64>,
    /// Why there is nothing to report, when there is nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(clap::Args, serde::Serialize, serde::Deserialize, schemars::JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FleetUpdateStatusArgs {}

/// Report the outcome of the most recent fleet roll on this host.
///
/// A fleet roll takes longer than an MCP client's idle timeout, so the client
/// aborts and the operator is left genuinely unsure what applied — the call
/// reported failure while having done real, partial work (#626). The roll
/// already writes a run record after EVERY host; nothing could read it back, so
/// recovering the outcome meant SSH and a JSON file by hand.
///
/// Read-only: it reports what a roll recorded, and never starts or resumes one.
#[orca_tool(domain = "system", verb = "update.status")]
async fn fleet_update_status(
    _args: FleetUpdateStatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<FleetUpdateStatusOutput> {
    let Some(dir) =
        contract::config::paths::orca_home().map(|h| h.join(contract::config::APP_LOGS_SUBDIR))
    else {
        return Ok(FleetUpdateStatusOutput {
            note: Some("no orca home on this host, so no run records are kept".into()),
            ..Default::default()
        });
    };
    let Some(path) = newest_record_in(&dir) else {
        return Ok(FleetUpdateStatusOutput {
            note: Some(format!(
                "no fleet-update run record under {}",
                dir.display()
            )),
            ..Default::default()
        });
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read run record {}", path.display()))?;
    #[allow(clippy::disallowed_types)]
    let run: crate::fleet::FleetUpdateOutput = serde_json::from_str(&raw)
        .with_context(|| format!("parse run record {}", path.display()))?;
    let record_age_secs = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs());
    Ok(FleetUpdateStatusOutput {
        run: Some(run),
        run_record: Some(path.display().to_string()),
        record_age_secs,
        note: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the plugin APPLY path to sequential dispatch.
    ///
    /// Structural for the same reason as the gate test below: the failure is a
    /// race inside a remote plugin manager, which cannot be reproduced
    /// in-process where a local target never crosses the wire.
    ///
    /// The probe path is deliberately concurrent (#626 — serial per-plugin
    /// release lookups are what made a read-only fleet probe outlast an MCP
    /// client's 300s idle timeout). The apply path must NOT follow it: two
    /// concurrent installs on one host race each other, and the speed is not
    /// worth it. The two paths sit next to each other in `run_plugins`, so the
    /// tempting edit is to "finish the job" and parallelise both.
    #[test]
    fn the_plugin_apply_path_stays_sequential() {
        let src = include_str!("fleet_update.rs");
        let body = src
            .split_once("async fn run_plugins(")
            .expect("run_plugins present")
            .1;
        let body = body.split_once("\nasync fn ").expect("next fn").0;
        let (apply, probe) = body
            .split_once("let probes = installed.iter()")
            .expect("probe path present");
        assert!(
            !apply.contains("join_all"),
            "the plugin APPLY path must stay sequential — concurrent installs \
             on one host race the plugin manager"
        );
        assert!(
            probe.contains("join_all"),
            "the plugin PROBE path must stay concurrent (#626)"
        );
    }

    #[test]
    fn skipping_plugins_is_stated_not_left_to_inference() {
        // An empty plugin list is indistinguishable from "this fleet has no
        // plugins". A skipped phase has to say so, or the output quietly
        // misreports what was examined.
        let src = include_str!("fleet_update.rs");
        assert!(
            src.contains("plugin phase SKIPPED"),
            "`--daemons-only` must record that it skipped the phase"
        );
    }

    /// Pins the gate's probe to the AUTHENTICATED dispatch path.
    ///
    /// A structural test rather than a behavioural one, deliberately: the bug it
    /// guards is a *missing credential*, and the only observable difference is a
    /// refusal from a remote peer — which cannot be reproduced in-process, since a
    /// local target dispatches via `OrcaTool::run` and never crosses the wire at
    /// all. So the thing worth pinning is the call itself.
    ///
    /// History: the probe used `peer_info::peer_update` -> `exec_peer`, whose
    /// signature takes no `ToolCtx` and therefore cannot present a signed caller
    /// token. Every peer enforcing `role = admin` on `system.update` refused it,
    /// so this gate had never verified a peer on any host, and the refusal was
    /// retried for the full 180s timeout — an inert safety check that looked
    /// merely slow. Swapping the body back to either function would silently
    /// restore that, with no failing test and no visible symptom beyond a gate
    /// that always times out.
    #[test]
    fn health_gate_probes_through_the_authenticated_dispatch_path() {
        let src = include_str!("fleet_update.rs");
        // Split at the signature so the doc comment above it — which names both
        // forbidden functions while explaining why they are forbidden — is not
        // itself mistaken for a call.
        let after_sig = src
            .split("async fn health_gate(")
            .nth(1)
            .expect("health_gate must exist");
        let body = after_sig
            .split("\nasync fn ")
            .next()
            .expect("health_gate body");

        assert!(
            body.contains("dispatch_at::<crate::commands::SystemUpdate>"),
            "the gate must probe through `dispatch_at`, which carries the ToolCtx \
             the peer needs to authorize the call"
        );
        for forbidden in ["peer_update", "exec_peer"] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` takes no ToolCtx, so the probe cannot present a \
                 caller token and every peer enforcing admin on system.update \
                 refuses it — that is the bug this test exists to prevent"
            );
        }
    }

    #[test]
    fn run_record_filename_is_shell_safe_and_sorts_chronologically() {
        let a = run_record_filename(
            &utils::time::Timestamp::parse_rfc3339("2026-09-26T04:05:54Z").unwrap(),
        );
        assert!(!a.contains(':'), "colons are awkward in shells: {a}");
        assert!(
            a.starts_with("fleet-update-") && a.ends_with(".json"),
            "{a}"
        );
        let b = run_record_filename(
            &utils::time::Timestamp::parse_rfc3339("2026-09-26T05:00:00Z").unwrap(),
        );
        assert!(a < b, "records must sort chronologically: {a} !< {b}");
    }

    /// The record is the durable copy of a roll whose last act severs the
    /// caller's connection, so it must be valid JSON that reads back.
    #[test]
    fn persist_writes_a_reloadable_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.json");
        let mut out = FleetUpdateOutput::default();
        out.notes.push("frigg: healthy on 0.2.1-rc.5".into());
        out.errors.push("bragi: unreachable".into());
        persist(&out, Some(&path));

        let back: FleetUpdateOutput =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.notes, out.notes);
        assert_eq!(back.errors, out.errors);
    }

    #[test]
    fn persist_without_a_path_is_a_noop_not_a_panic() {
        persist(&FleetUpdateOutput::default(), None);
    }

    /// Structural guard for #625 and #649 together: the LOCAL apply is the last
    /// work the fan-out does, and the report is flushed before it.
    ///
    /// Both properties are about the same instruction — the local `run_system`
    /// is the one that kills this process — so they are pinned at the same
    /// place. If a refactor moves the plugin phase after it, plugin updates
    /// resume racing a 2-second SIGTERM (#649); if it moves the flush after it,
    /// the report is lost again (#625). Neither has a visible symptom, and both
    /// only bite on the one run that matters.
    #[test]
    fn local_apply_is_last_and_preceded_by_a_persist() {
        let src = include_str!("fleet_update.rs");
        let body = src
            .split("pub async fn fleet_update(")
            .nth(1)
            .expect("fleet_update present");
        // Bound the scan to THIS function. It used to run to end-of-file, so
        // any later code — a sibling fn, or a test merely naming `run_plugins`
        // in a string — counted as "after the local apply" and failed this for
        // a reason that had nothing to do with the ordering it pins.
        let body = body
            .split_once("\nasync fn ")
            .map(|(f, _)| f)
            .unwrap_or(body);
        let phase2 = body
            .find("// ── PHASE 2")
            .expect("plugin phase marker present");
        let phase3 = body
            .find("// ── PHASE 3")
            .expect("local-apply phase marker present");
        assert!(
            phase2 < phase3,
            "the plugin phase must complete BEFORE the local apply: applying \
             locally schedules a 2s detached restart, so anything after it is a race"
        );
        let local_apply = body[phase3..]
            .find("let row = run_system(t, true, ctx).await")
            .map(|i| i + phase3)
            .expect("local apply call present");
        assert!(
            body[phase3..local_apply].contains("persist(&out, record.as_deref())"),
            "the local apply must be preceded by a flush of the run record"
        );
        assert!(
            !body[local_apply..].contains("run_plugins("),
            "no plugin work may follow the local apply"
        );
    }

    /// A dry run must never take the fleet lock, and must never be refused by
    /// one: it applies nothing, so it is not a roll (#616). The natural way to
    /// ask "what version is every host on" cannot be blocked by a roll in
    /// flight — that is exactly when an operator most wants to ask.
    #[test]
    fn only_an_execute_takes_the_fleet_lock() {
        let src = include_str!("fleet_update.rs");
        let body = src
            .split("pub async fn fleet_update(")
            .nth(1)
            .expect("fleet_update present");
        let acquire = body
            .find("acquire_fleet_lock(")
            .expect("lock acquisition present");
        let guard = body[..acquire]
            .rfind("let _lock = if execute {")
            .expect("lock must be taken only under `if execute`");
        assert!(guard < acquire);
    }

    #[test]
    fn lock_is_stale_only_past_the_ttl() {
        let start = "2026-09-26T04:00:00Z";
        let at = |s: &str| utils::time::Timestamp::parse_rfc3339(s).unwrap();
        // Mid-roll: a long but live roll must not be broken into.
        assert!(!lock_is_stale(start, &at("2026-09-26T05:29:00Z")));
        // Past the TTL: a crashed controller must not block the fleet forever.
        assert!(lock_is_stale(start, &at("2026-09-26T06:00:00Z")));
    }

    /// An unreadable timestamp must read as HELD, not free. Refusing a second
    /// roll is recoverable (`--break-lock`); two concurrent rolls are not.
    #[test]
    fn an_unparseable_lock_timestamp_is_treated_as_held() {
        let now = utils::time::now();
        assert!(!lock_is_stale("", &now));
        assert!(!lock_is_stale("not a timestamp", &now));
    }

    /// "Already running" with no holder is unactionable. The refusal must name
    /// who holds it and since when, so an operator can decide between waiting
    /// and breaking it (#616).
    #[test]
    fn lock_refusal_names_the_holder_and_when_it_started() {
        let msg = lock_refusal(&LockRecord {
            host: "mint".into(),
            pid: Some(4242),
            started_at: "2026-09-26T04:05:54Z".into(),
        });
        assert!(msg.contains("mint"), "{msg}");
        assert!(msg.contains("4242"), "{msg}");
        assert!(msg.contains("2026-09-26T04:05:54Z"), "{msg}");
        assert!(msg.contains("--break-lock"), "{msg}");
    }

    /// A lock file missing its fields must still produce a usable refusal
    /// rather than panicking or claiming a holder it cannot name.
    #[test]
    fn lock_refusal_degrades_without_fields() {
        let msg = lock_refusal(&LockRecord::default());
        assert!(msg.contains("unknown"), "{msg}");
        assert!(msg.contains("--break-lock"), "{msg}");
    }

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
        use crate::update_state::Channel;
        // Explicit flag always wins.
        assert!(resolve_prerelease(true, Channel::Stable));
        assert!(resolve_prerelease(true, Channel::Beta));
        // Unset defaults to the daemon channel: beta ⇒ prerelease, stable ⇒ not.
        assert!(resolve_prerelease(false, Channel::Beta));
        assert!(!resolve_prerelease(false, Channel::Stable));
    }

    #[tokio::test]
    async fn dispatch_at_local_runs_in_process_never_mesh_exec() {
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
    fn bare_orca_update_is_a_cli_alias_for_system_update() {
        let root = dispatch::cli::build_root(clap::Command::new("orca"));
        // `orca update` still parses. It no longer overrides any default:
        // updating every system IS the default when no `--id` is given, so the
        // alias and `orca system update` now mean exactly the same thing.
        let m = root
            .clone()
            .try_get_matches_from(["orca", "update"])
            .expect("`orca update` must parse");
        let (name, sub) = m.subcommand().expect("update subcommand present");
        assert_eq!(name, "update");
        assert_eq!(sub.get_one::<String>("id"), None);
        // `--scope` is gone from the CLI entirely.
        assert!(
            root.clone()
                .try_get_matches_from(["orca", "update", "--scope", "fleet"])
                .is_err(),
            "`--scope` must no longer parse"
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
        fn assert_hook<T: crate::fleet::FleetUpdateHook>() {}
        assert_hook::<MeshFleetUpdateHook>();
    }

    // ── #618 controller readiness and blame ────────────────────────────────

    fn errored(host: &str, err: &str) -> FleetSystemResult {
        FleetSystemResult {
            host: host.into(),
            id: format!("id-{host}"),
            error: Some(err.into()),
            ..Default::default()
        }
    }

    #[test]
    fn every_peer_unreached_is_reported_as_one_controller_fault() {
        let rows = [
            errored(
                "a",
                "connect 10.0.0.1:12002: No route to host (os error 65)",
            ),
            errored(
                "b",
                "connect 10.0.0.2:12002: No route to host (os error 65)",
            ),
        ];
        let refs: Vec<&FleetSystemResult> = rows.iter().collect();
        let diag = all_peers_unreached(&refs).expect("should diagnose locally");
        assert!(diag.contains("CONTROLLER-side"), "{diag}");
        assert!(diag.contains("all 2 remote hosts"), "{diag}");
    }

    #[test]
    fn a_peer_that_answered_and_refused_is_not_a_controller_fault() {
        // A refusal proves the connection got there, so the fault is genuinely
        // on that side and must keep being reported per-host.
        let rows = [
            errored(
                "a",
                "connect 10.0.0.1:12002: No route to host (os error 65)",
            ),
            errored(
                "b",
                "mesh/exec refused: tool 'system.update' requires role 'admin'",
            ),
        ];
        let refs: Vec<&FleetSystemResult> = rows.iter().collect();
        assert!(all_peers_unreached(&refs).is_none());
    }

    #[test]
    fn one_unreachable_peer_is_that_peers_problem_not_the_controllers() {
        let rows = [errored("a", "connect 10.0.0.1:12002: No route to host")];
        let refs: Vec<&FleetSystemResult> = rows.iter().collect();
        assert!(all_peers_unreached(&refs).is_none());
    }

    #[test]
    fn a_partially_successful_roll_is_never_blamed_on_the_controller() {
        let ok = FleetSystemResult {
            host: "a".into(),
            id: "id-a".into(),
            applied: Some("0.2.1".into()),
            ..Default::default()
        };
        let rows = [ok, errored("b", "No route to host")];
        let refs: Vec<&FleetSystemResult> = rows.iter().collect();
        assert!(all_peers_unreached(&refs).is_none());
    }

    #[test]
    fn dial_readiness_refuses_when_mesh_material_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let refusal =
            controller_dial_readiness(dir.path(), "0.2.1").expect("an empty pki dir cannot dial");
        // Must point at the controller, not the targets — that misdirection is
        // the whole defect in #618.
        assert!(refusal.contains("controller is not ready"), "{refusal}");
        assert!(refusal.contains("LOCAL"), "{refusal}");
    }

    // ── #623 gate skew ─────────────────────────────────────────────────────

    #[test]
    fn rolling_peers_past_the_controller_discloses_the_gate_blind_spot() {
        let note = gate_skew_note("0.2.1-rc.6", &["0.2.1-rc.7"]).expect("skew present");
        assert!(note.contains("gate-skew"), "{note}");
        assert!(note.contains("NOT exercised"), "{note}");
    }

    #[test]
    fn no_gate_skew_when_the_controller_is_already_on_the_rolled_version() {
        assert!(gate_skew_note("0.2.1-rc.7", &["0.2.1-rc.7"]).is_none());
        // A leading `v` is a spelling difference, not a version difference.
        assert!(gate_skew_note("0.2.1-rc.7", &["v0.2.1-rc.7"]).is_none());
    }

    #[test]
    fn no_gate_skew_when_nothing_was_applied() {
        assert!(gate_skew_note("0.2.1-rc.7", &[]).is_none());
    }

    #[test]
    fn the_newest_record_is_the_one_reported() {
        // An operator whose client aborted wants THIS roll, not last week's.
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "fleet-update-2026-09-26T04-05-54Z.json",
            "fleet-update-2026-09-29T02-53-40Z.json",
            "fleet-update-2026-09-27T19-43-21Z.json",
        ] {
            std::fs::write(dir.path().join(name), "{}").unwrap();
        }
        let got = newest_record_in(dir.path()).unwrap();
        assert!(
            got.ends_with("fleet-update-2026-09-29T02-53-40Z.json"),
            "got: {}",
            got.display()
        );
    }

    #[test]
    fn unrelated_files_in_the_log_dir_are_not_run_records() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.log"), "x").unwrap();
        std::fs::write(dir.path().join("fleet-update-notes.txt"), "x").unwrap();
        assert!(newest_record_in(dir.path()).is_none());
    }

    #[test]
    fn an_empty_log_dir_reports_nothing_rather_than_failing() {
        // "No roll has run here" is an answer, not an error.
        let dir = tempfile::tempdir().unwrap();
        assert!(newest_record_in(dir.path()).is_none());
    }

    /// The whole point is recovering a roll the caller lost, so the record a
    /// real roll writes must read back through the status path.
    #[test]
    fn a_persisted_roll_reads_back_as_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-update-2026-09-29T02-53-40Z.json");
        let out = crate::fleet::FleetUpdateOutput {
            dry_run: false,
            notes: vec!["thor: applied 0.2.1-rc.8".into()],
            ..Default::default()
        };
        persist(&out, Some(&path));

        let found = newest_record_in(dir.path()).expect("record found");
        let raw = std::fs::read_to_string(&found).unwrap();
        let back: crate::fleet::FleetUpdateOutput = serde_json::from_str(&raw).unwrap();
        assert!(!back.dry_run);
        assert_eq!(back.notes, vec!["thor: applied 0.2.1-rc.8".to_string()]);
    }
}
