//! Consumer-aware bind-mount self-heal, shared by network-storage backends.
//!
//! A host mount can recover (autofs re-triggers, a fresh superblock lands) while
//! a long-running container that bind-mounted a subpath still pins the OLD
//! superblock — reading the bind ROOT inside that container returns ESTALE even
//! though the host is healthy. The fix is to restart the pinning consumer so it
//! re-binds the fresh mount. That logic is **fstype-agnostic** — it operates on
//! watched host-path prefixes and docker/pct binds — so it lives here in
//! `plugin-toolkit` and is shared by the `nfs` and `smb` storage backends rather
//! than duplicated in each.
//!
//! The host-mount sweep itself (probe → force-release → remount) stays in each
//! backend, which owns its own fstype grammar; this module is only the consumer
//! (guest) half. A backend calls [`recover_consumers_for_owner`] with its detected
//! runtimes and a `host_healthy` closure computed from its own mount table.
//!
//! A restart that leaves a consumer stopped (its bind source vanished) is
//! recorded in a per-owner ledger, `<orca home>/state/mount_recover_stopped.<owner>.json`,
//! and started again once its host source has probed healthy for
//! [`RESTORE_CONFIRM_SWEEPS`] consecutive sweeps. An entry is dropped when the
//! consumer is started, restopped, or removed by someone else, when it is in a
//! state a start cannot fix (`dead`/`created`), after [`MAX_START_FAILURES`]
//! failed starts, or when it carries the opt-out label
//! [`NO_RESTORE_LABEL`]`=true`.
//! Its sibling `.json.lock` file is permanent by design: deleting it would race
//! a concurrent sweep that already holds the old inode's lock.

use std::path::{Path, PathBuf};
use std::time::Duration;

use derive::{orca_async, orca_error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::process::Command;

/// Tool / transport errors raised while sweeping container consumers.
#[orca_error]
pub enum RecoverError {
    /// A runtime shell-out (`docker`/`pct`) exited non-zero.
    #[orca(display = "{tool} failed (exit {code:?}): {stderr}")]
    ToolFailed {
        tool: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    /// Spawning / awaiting the runtime process failed.
    #[orca(display = "io: {0}", from)]
    Io(std::io::Error),
    /// The runtime does not implement this operation.
    #[orca(display = "{0} unsupported by this runtime")]
    Unsupported(&'static str),
}

/// Classify a `stat` failure's stderr as `"stale"` (the bind's underlying
/// session/handle is dead — restart the consumer) or `"error: <stderr>"`
/// (something else — do not restart). Covers BOTH network fstypes this module
/// serves, because a wedged bind surfaces differently per protocol:
///
/// - **NFS** fast-fails only with ESTALE — `"Stale file handle"`.
/// - **CIFS/SMB** drops the session with a wider errno set: `EBADF`
///   (`"Bad file descriptor"` — the exact willow-flap signature that wedged
///   sabnzbd), `EHOSTDOWN` (`"Host is down"`), `ENOTCONN`
///   (`"Transport endpoint is not connected"` / `"Not connected"`), `EIO`
///   (`"Input/output error"`), and a hung `EAGAIN`
///   (`"Resource temporarily unavailable"`).
///
/// Every one of these means the mount behind the bind is gone, and the guard in
/// [`recover_stale_consumers`] only reaches a restart when the HOST mount is
/// healthy — so a broad match here cannot storm on a host-wide outage. A plain
/// `ENOENT` / `EACCES` is NOT a session flap and stays a plain `error:`. Pure +
/// case-insensitive so it is unit-testable without spawning `stat`.
pub fn classify_stat_failure(stderr: &str) -> String {
    // Stderr fragments (case-insensitive) that mean the bind's session/handle is
    // dead across NFS and CIFS. `"transport endpoint is not connected"` is
    // covered by the shorter `"not connected"` fragment.
    const STALE_SIGNATURES: &[&str] = &[
        "stale file handle",
        "bad file descriptor",
        "host is down",
        "not connected",
        "input/output error",
        "resource temporarily unavailable",
    ];
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if STALE_SIGNATURES.iter().any(|sig| lower.contains(sig)) {
        "stale".to_string()
    } else {
        format!("error: {trimmed}")
    }
}

/// Does a host path fall under one of the watched prefixes? `/foo` matches `/foo`
/// and `/foo/...` but not `/foobar`. An empty watch set matches everything.
pub fn path_under_watch(path: &str, watch: &[String]) -> bool {
    if watch.is_empty() {
        return true;
    }
    watch.iter().any(|w| match path.strip_prefix(w.as_str()) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    })
}

/// One host→container bind of a watched path, as seen by a container runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerBind {
    /// Container id (runtime-native handle used for exec/restart).
    pub container_id: String,
    /// Human-friendly container name for reporting.
    pub container_name: String,
    /// The host path being bind-mounted (matches a watched prefix).
    pub host_source: String,
    /// The path the bind is mounted at *inside* the container — the ROOT probed
    /// for ESTALE.
    pub container_target: String,
}

/// Result of a `stat` probe of a bind ROOT inside a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerProbe {
    Ok,
    Stale,
}

/// Structured outcome of a consumer sweep: consumers are categorized so the
/// caller can log and continue. Carries serde + schema derives so a backend can
/// either fold it into the wire `RecoverOutcome`'s `Vec<String>` fields (smb) or
/// nest it directly in its own serialized result type (nfs's `RecoverResult`).
/// (`#[orca_struct]` cannot be used here — it expands to self-referential
/// `plugin_toolkit::` paths that don't resolve inside the toolkit crate — so the
/// derives are spelled out as the sibling `storage` types do.)
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConsumerRecoverResult {
    /// Containers whose bind ROOT probed healthy — nothing to do.
    pub healthy: Vec<String>,
    /// Containers that were ESTALE and were restarted back to health, plus
    /// `started:<name>` entries for stopped guests auto-started by the gate.
    pub recovered: Vec<String>,
    /// Containers ESTALE but NOT restarted because the covering host mount was
    /// itself stale (host-wide outage guard) — a restart would storm, not help.
    pub skipped_host_stale: Vec<String>,
    /// Containers restarted but still ESTALE afterwards, or whose restart failed.
    pub still_stale: Vec<String>,
    /// Non-fatal per-consumer errors (enumerate/probe/restart failures).
    pub errors: Vec<String>,
    /// `true` when no watched bind was found at all (fast path / no-op).
    pub no_consumers_found: bool,
}

/// Abstraction over the container runtime so the consumer sweep is testable
/// without Docker/Proxmox. Production impls ([`DockerCli`], [`PctCli`]) shell
/// out behind this trait, confining every runtime shell-out to one swappable
/// seam rather than scattering `Command::new(..)` through the sweep logic.
#[orca_async]
pub trait ContainerRuntime: Send + Sync {
    /// Enumerate containers bind-mounting any host path under one of `watch`.
    async fn binds_under(&self, watch: &[String]) -> Result<Vec<ConsumerBind>, RecoverError>;

    /// Probe `path` inside container `id` with a timeout. ESTALE (or a hang past
    /// the budget) → [`ConsumerProbe::Stale`]; success → `Ok`. Any other failure
    /// is surfaced as `Err` for the caller to record.
    async fn probe_path(
        &self,
        id: &str,
        path: &str,
        timeout: Duration,
    ) -> Result<ConsumerProbe, RecoverError>;

    /// Restart container `id` to re-bind the fresh mount.
    async fn restart(&self, id: &str) -> Result<(), RecoverError>;

    /// Start any consumer that is currently STOPPED, declares a bind of a watched
    /// host path, and whose covering host mount is healthy — the orca-owned
    /// replacement for a host-local "refuse start until the mount is live" hook
    /// that never retries. `host_healthy(host_source)` gates the start exactly
    /// like the restart path, so a host-wide outage never starts a fleet of
    /// guests against a dead mount. Returns the names started. Runtimes with no
    /// notion of a stopped-but-configured consumer (e.g. Docker) keep the
    /// default no-op.
    async fn start_gated(
        &self,
        _watch: &[String],
        _host_healthy: &(dyn Fn(&str) -> bool + Sync),
    ) -> Vec<String> {
        Vec::new()
    }

    /// Stable runtime label keying [`StoppedConsumer`] records.
    fn name(&self) -> &'static str {
        ""
    }

    /// Current state of consumer `id`; `None` when the runtime cannot tell, in
    /// which case nothing is recorded for it.
    async fn state(&self, _id: &str) -> Result<Option<ConsumerState>, RecoverError> {
        Ok(None)
    }

    /// Start stopped consumer `id`.
    async fn start(&self, _id: &str) -> Result<(), RecoverError> {
        Err(RecoverError::Unsupported("start"))
    }
}

/// Is `bin` on `PATH`? Used to pick which container runtimes to sweep — a host
/// may run Docker, Proxmox (`pct`), both, or neither.
async fn have_binary(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .await
        .map(|o| o.status.success)
        .unwrap_or(false)
}

/// Detect the container runtimes present on this host, in a stable order. Empty
/// when neither Docker nor Proxmox is installed (recovery is then host-only).
pub async fn detect_runtimes() -> Vec<Box<dyn ContainerRuntime>> {
    let mut runtimes: Vec<Box<dyn ContainerRuntime>> = Vec::new();
    if have_binary("docker").await {
        runtimes.push(Box::new(DockerCli));
    }
    if have_binary("pct").await {
        runtimes.push(Box::new(PctCli));
    }
    runtimes
}

/// Sweep one runtime's consumers: enumerate binds of watched paths, probe each
/// bind ROOT inside its container, and — only when the covering host mount is
/// healthy — restart any consumer that is ESTALE. Guarded so a host-wide outage
/// never triggers a restart storm.
pub async fn recover_stale_consumers<F>(
    runtime: &dyn ContainerRuntime,
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> bool,
{
    sweep_runtime(
        runtime,
        watch,
        health_timeout,
        host_healthy,
        &mut Vec::new(),
    )
    .await
}

/// [`recover_stale_consumers`], additionally recording into `stopped` every
/// consumer the restart left stopped.
async fn sweep_runtime<F>(
    runtime: &dyn ContainerRuntime,
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
    stopped: &mut Vec<StoppedConsumer>,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> bool,
{
    let mut result = ConsumerRecoverResult::default();

    let binds = match runtime.binds_under(watch).await {
        Ok(b) => b,
        Err(e) => {
            result.errors.push(format!("enumerate consumer binds: {e}"));
            return result;
        }
    };
    if binds.is_empty() {
        result.no_consumers_found = true;
        return result;
    }

    for bind in &binds {
        // Probe the bind ROOT as seen inside the consumer.
        match runtime
            .probe_path(&bind.container_id, &bind.container_target, health_timeout)
            .await
        {
            Ok(ConsumerProbe::Ok) => result.healthy.push(bind.container_name.clone()),
            Ok(ConsumerProbe::Stale) => {
                // Guard: only remediate a stale bind when the HOST mount is
                // healthy. A host-wide outage makes every consumer stale;
                // restarting then is pointless and stormy.
                if !host_healthy(&bind.host_source) {
                    result.skipped_host_stale.push(bind.container_name.clone());
                    continue;
                }
                let restarted = runtime.restart(&bind.container_id).await;
                // `docker restart` stops before it starts; a start refused by a
                // vanished bind source leaves the consumer down for good. Checked
                // after every restart because the exit status alone does not say
                // whether the consumer ended up running (e.g. a stop timeout).
                if let Ok(Some(ConsumerState::Stopped { finished_at, .. })) =
                    runtime.state(&bind.container_id).await
                {
                    stopped.push(StoppedConsumer {
                        runtime: runtime.name().to_string(),
                        container_id: bind.container_id.clone(),
                        container_name: bind.container_name.clone(),
                        host_source: bind.host_source.clone(),
                        finished_at,
                        healthy_sweeps: 0,
                        start_failures: 0,
                    });
                }
                match restarted {
                    Ok(()) => match runtime
                        .probe_path(&bind.container_id, &bind.container_target, health_timeout)
                        .await
                    {
                        Ok(ConsumerProbe::Ok) => result.recovered.push(bind.container_name.clone()),
                        Ok(ConsumerProbe::Stale) => {
                            result.still_stale.push(bind.container_name.clone())
                        }
                        Err(e) => {
                            result.still_stale.push(bind.container_name.clone());
                            result
                                .errors
                                .push(format!("re-probe {}: {e}", bind.container_name));
                        }
                    },
                    Err(e) => {
                        result.still_stale.push(bind.container_name.clone());
                        result
                            .errors
                            .push(format!("restart {}: {e}", bind.container_name));
                    }
                }
            }
            Err(e) => result
                .errors
                .push(format!("probe {}: {e}", bind.container_name)),
        }
    }

    result
}

/// Full consumer self-heal across MANY runtimes: every runtime's consumer sweep
/// (stale-bind restart) and start-gate (stopped-guest auto-start) runs against a
/// shared `host_healthy` snapshot the caller supplies, folding all outcomes into
/// one [`ConsumerRecoverResult`]. A backend runs its own host-mount sweep first,
/// then calls this with `detect_runtimes()` and a `host_healthy` closure computed
/// from its post-recovery mount table.
///
/// `owner` names the calling backend (e.g. `"nfs"`) and keys its own ledger, so
/// backends sharing a host never touch each other's records. `host_healthy`
/// answers `None` for a source the owner does not manage: nothing is restarted,
/// started, or counted for it.
///
/// `owner` must match `[a-z0-9_-]{1,32}`; anything else is rejected with an error.
pub async fn recover_consumers_for_owner<F>(
    owner: &str,
    runtimes: &[Box<dyn ContainerRuntime>],
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> Option<bool> + Sync,
{
    let home = utils::home::orca_home();
    recover_consumers_for_owner_at(
        owner,
        runtimes,
        watch,
        health_timeout,
        host_healthy,
        home.as_deref(),
    )
    .await
}

/// Single-ledger form of [`recover_consumers_for_owner`] under owner `"legacy"`.
#[deprecated(note = "use recover_consumers_for_owner")]
pub async fn recover_consumers_multi<F>(
    runtimes: &[Box<dyn ContainerRuntime>],
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> bool + Sync,
{
    recover_consumers_for_owner(LEGACY_OWNER, runtimes, watch, health_timeout, |s| {
        Some(host_healthy(s))
    })
    .await
}

const LEGACY_OWNER: &str = "legacy";

fn valid_owner(owner: &str) -> bool {
    (1..=32).contains(&owner.len())
        && owner
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

async fn recover_consumers_for_owner_at<F>(
    owner: &str,
    runtimes: &[Box<dyn ContainerRuntime>],
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
    home: Option<&Path>,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> Option<bool> + Sync,
{
    let mut merged = ConsumerRecoverResult {
        no_consumers_found: true,
        ..Default::default()
    };
    if !valid_owner(owner) {
        merged.errors.push(format!(
            "invalid owner {owner:?}: must match [a-z0-9_-]{{1,32}}"
        ));
        return merged;
    }
    let ledger_path = home.map(|h| stopped_ledger_file(h, owner));
    if ledger_path.is_none() {
        merged
            .errors
            .push("no orca home resolved: stopped consumers are not tracked or restored".into());
    }
    // Held across load→save so a concurrent sweep of the same owner cannot
    // drop this run's records.
    let mut locked = ledger_path.as_deref().map(lock_ledger).transpose();
    for _ in 0..LOCK_RETRIES {
        if !matches!(locked, Ok(Some(None))) {
            break;
        }
        crate::time::sleep(LOCK_RETRY_DELAY).await;
        locked = ledger_path.as_deref().map(lock_ledger).transpose();
    }
    let _lock = match locked {
        Ok(Some(None)) => {
            merged.errors.push(format!(
                "mount recovery sweep already in progress for {owner}"
            ));
            return merged;
        }
        Ok(l) => l,
        Err(e) => {
            merged
                .errors
                .push(format!("lock stopped-consumer ledger: {e}"));
            return merged;
        }
    };
    let mut ledger = ledger_path
        .as_deref()
        .map(|p| load_stopped_at(p, &mut merged.errors))
        .unwrap_or_default();
    let before = ledger.clone();
    let healthy_bool = |s: &str| host_healthy(s) == Some(true);
    for rt in runtimes {
        restore_stopped(rt.as_ref(), &mut ledger, &host_healthy, &mut merged).await;
        let mut stopped = Vec::new();
        let c = sweep_runtime(
            rt.as_ref(),
            watch,
            health_timeout,
            healthy_bool,
            &mut stopped,
        )
        .await;
        for s in stopped {
            ledger.retain(|e| !(e.runtime == s.runtime && e.container_id == s.container_id));
            ledger.push(s);
        }
        if !c.no_consumers_found {
            merged.no_consumers_found = false;
        }
        merged.healthy.extend(c.healthy);
        merged.recovered.extend(c.recovered);
        merged.skipped_host_stale.extend(c.skipped_host_stale);
        merged.still_stale.extend(c.still_stale);
        merged.errors.extend(c.errors);

        // Hook replacement: start stopped guests whose managed bind's host mount
        // is now healthy. Reported as `started:<name>` in `recovered`.
        let started = rt.start_gated(watch, &healthy_bool).await;
        if !started.is_empty() {
            merged.no_consumers_found = false;
            merged
                .recovered
                .extend(started.into_iter().map(|n| format!("started:{n}")));
        }
    }
    if let Some(path) = ledger_path.as_deref()
        && ledger != before
        && let Err(e) = save_stopped_at(path, &ledger)
    {
        merged
            .errors
            .push(format!("persist stopped-consumer ledger: {e}"));
    }
    merged
}

/// Consecutive sweeps a recorded consumer's host source must probe healthy
/// before it is started again, so a flapping mount never bounces it.
pub const RESTORE_CONFIRM_SWEEPS: u32 = 3;

/// Failed starts after which a recorded consumer is given up on.
pub const MAX_START_FAILURES: u32 = 5;

/// Container label that opts a consumer out of being restored; checked before
/// every start.
pub const NO_RESTORE_LABEL: &str = "orca.mount_recover.no-restore";

/// A consumer this sweep's restart left stopped, pending a start once its host
/// source recovers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedConsumer {
    pub runtime: String,
    pub container_id: String,
    pub container_name: String,
    pub host_source: String,
    /// The runtime's last-stop timestamp when recorded. It identifies this
    /// particular stop: a different value later means someone else started and
    /// stopped it since, so it is no longer ours.
    pub finished_at: String,
    pub healthy_sweeps: u32,
    #[serde(default)]
    pub start_failures: u32,
}

/// Live state of a consumer, as far as a runtime can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsumerState {
    Gone,
    /// Running, or in a transition (paused/restarting/removing) someone else owns.
    Running,
    Stopped {
        finished_at: String,
        no_restore: bool,
    },
    /// Present but in a state a start cannot fix (e.g. `dead`, `created`).
    Unrestorable {
        status: String,
    },
}

fn stopped_ledger_file(home: &Path, owner: &str) -> PathBuf {
    home.join("state")
        .join(format!("mount_recover_stopped.{owner}.json"))
}

const LOCK_RETRIES: u32 = 20;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

/// `Ok(None)` when another sweep holds the lock; never blocks the caller's thread.
fn lock_ledger(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Missing → empty. Unreadable/corrupt → an error in `errors`, the file moved
/// aside to `.corrupt.<nanos>` for inspection, and an empty ledger.
fn load_stopped_at(path: &Path, errors: &mut Vec<String>) -> Vec<StoppedConsumer> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            errors.push(format!(
                "read stopped-consumer ledger {}: {e}",
                path.display()
            ));
            return Vec::new();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(l) => l,
        Err(e) => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default();
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt.{nanos}"));
            let moved = std::fs::rename(path, &aside)
                .map(|()| format!("moved to {}", Path::new(&aside).display()))
                .unwrap_or_else(|re| format!("could not move aside: {re}"));
            errors.push(format!(
                "corrupt stopped-consumer ledger {}: {e}; {moved}",
                path.display()
            ));
            Vec::new()
        }
    }
}

fn save_stopped_at(path: &Path, ledger: &[StoppedConsumer]) -> anyhow::Result<()> {
    utils::atomic::write_mkdir(path, &serde_json::to_vec_pretty(ledger)?)?;
    // The rename is durable only once the directory entry is.
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Start `rt`'s recorded consumers whose host source has stayed healthy for
/// [`RESTORE_CONFIRM_SWEEPS`]; drop any that were started, stopped again, or
/// removed by someone else. Reported as `restored:<name>` in `recovered`.
async fn restore_stopped(
    rt: &dyn ContainerRuntime,
    ledger: &mut Vec<StoppedConsumer>,
    host_healthy: &(dyn Fn(&str) -> Option<bool> + Sync),
    result: &mut ConsumerRecoverResult,
) {
    let mut keep = Vec::with_capacity(ledger.len());
    for mut e in std::mem::take(ledger) {
        if e.runtime != rt.name() {
            keep.push(e);
            continue;
        }
        match rt.state(&e.container_id).await {
            Ok(Some(ConsumerState::Stopped {
                finished_at,
                no_restore,
            })) if finished_at == e.finished_at => {
                if no_restore {
                    result.errors.push(format!(
                        "{}: {NO_RESTORE_LABEL}=true, not restoring",
                        e.container_name
                    ));
                    continue;
                }
            }
            Ok(Some(ConsumerState::Unrestorable { status })) => {
                result.errors.push(format!(
                    "{} is {status}, not restorable; dropped",
                    e.container_name
                ));
                continue;
            }
            Ok(Some(_)) => continue,
            Ok(None) => {
                keep.push(e);
                continue;
            }
            Err(err) => {
                result
                    .errors
                    .push(format!("state {}: {err}", e.container_name));
                keep.push(e);
                continue;
            }
        }
        match host_healthy(&e.host_source) {
            None => {
                keep.push(e);
                continue;
            }
            Some(false) => {
                e.healthy_sweeps = 0;
                keep.push(e);
                continue;
            }
            Some(true) => {}
        }
        e.healthy_sweeps += 1;
        if e.healthy_sweeps < RESTORE_CONFIRM_SWEEPS {
            keep.push(e);
            continue;
        }
        match rt.start(&e.container_id).await {
            Ok(()) => result
                .recovered
                .push(format!("restored:{}", e.container_name)),
            Err(err) => {
                e.start_failures += 1;
                if e.start_failures >= MAX_START_FAILURES {
                    result.errors.push(format!(
                        "giving up on {} after {MAX_START_FAILURES} failed starts: {err}",
                        e.container_name
                    ));
                } else {
                    result
                        .errors
                        .push(format!("start {}: {err}", e.container_name));
                    keep.push(e);
                }
            }
        }
    }
    *ledger = keep;
}

/// Production [`ContainerRuntime`] that shells `docker`. All docker shell-outs
/// are confined here so the sweep logic stays runtime-agnostic and mockable.
pub struct DockerCli;

/// Tab-separated one-line-per-bind format emitted by `docker inspect`:
/// `id\tname\tsource\tdestination` for every `bind`-type mount.
const DOCKER_BIND_FORMAT: &str = "{{range .Mounts}}{{if eq .Type \"bind\"}}{{$.Id}}\t{{$.Name}}\t{{.Source}}\t{{.Destination}}\n{{end}}{{end}}";

#[orca_async]
impl ContainerRuntime for DockerCli {
    async fn binds_under(&self, watch: &[String]) -> Result<Vec<ConsumerBind>, RecoverError> {
        // Running container ids first, then inspect their bind mounts.
        let stdout = Command::new("docker")
            .arg("ps")
            .arg("--no-trunc")
            .arg("--format")
            .arg("{{.ID}}")
            .run_checked()
            .await
            .map_err(|e| RecoverError::ToolFailed {
                tool: "docker ps",
                code: e.code,
                stderr: e.stderr,
            })?;
        let ids: Vec<String> = String::from_utf8_lossy(&stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // `Command::arg` consumes `self` (builder), so fold the ids in.
        let mut inspect = Command::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(DOCKER_BIND_FORMAT);
        for id in &ids {
            inspect = inspect.arg(id);
        }
        let stdout = inspect
            .run_checked()
            .await
            .map_err(|e| RecoverError::ToolFailed {
                tool: "docker inspect",
                code: e.code,
                stderr: e.stderr,
            })?;
        Ok(parse_docker_binds(&String::from_utf8_lossy(&stdout), watch))
    }

    async fn probe_path(
        &self,
        id: &str,
        path: &str,
        timeout: Duration,
    ) -> Result<ConsumerProbe, RecoverError> {
        let fut = Command::new("docker")
            .arg("exec")
            .arg(id)
            .arg("stat")
            .arg("--")
            .arg(path)
            .output();
        match crate::time::timeout(timeout, fut).await {
            // In-container `stat` hung past the budget → stale (same rule as the
            // host probe's timeout→stale).
            None => Ok(ConsumerProbe::Stale),
            Some(Err(e)) => Err(RecoverError::Io(e)),
            Some(Ok(out)) if out.status.success => Ok(ConsumerProbe::Ok),
            Some(Ok(out)) => {
                if classify_stat_failure(&String::from_utf8_lossy(&out.stderr)) == "stale" {
                    Ok(ConsumerProbe::Stale)
                } else {
                    Err(RecoverError::Io(std::io::Error::other(
                        String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    )))
                }
            }
        }
    }

    async fn restart(&self, id: &str) -> Result<(), RecoverError> {
        Command::new("docker")
            .arg("restart")
            .arg(id)
            .run_checked()
            .await
            .map(|_stdout| ())
            .map_err(|e| RecoverError::ToolFailed {
                tool: "docker restart",
                code: e.code,
                stderr: e.stderr,
            })
    }

    fn name(&self) -> &'static str {
        "docker"
    }

    async fn state(&self, id: &str) -> Result<Option<ConsumerState>, RecoverError> {
        let out = Command::new("docker")
            .arg("inspect")
            .arg("--format")
            .arg(DOCKER_STATE_FORMAT)
            .arg(id)
            .output()
            .await?;
        if !out.status.success {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("No such") {
                return Ok(Some(ConsumerState::Gone));
            }
            return Err(RecoverError::ToolFailed {
                tool: "docker inspect",
                code: out.status.code,
                stderr: stderr.into_owned(),
            });
        }
        Ok(parse_docker_state(&String::from_utf8_lossy(&out.stdout)))
    }

    async fn start(&self, id: &str) -> Result<(), RecoverError> {
        Command::new("docker")
            .arg("start")
            .arg(id)
            .run_checked()
            .await
            .map(|_stdout| ())
            .map_err(|e| RecoverError::ToolFailed {
                tool: "docker start",
                code: e.code,
                stderr: e.stderr,
            })
    }
}

/// `status\tfinished_at\tno_restore_label` for [`DockerCli::state`]; `index` of
/// a missing label yields an empty field.
const DOCKER_STATE_FORMAT: &str = "{{.State.Status}}\t{{.State.FinishedAt}}\t{{index .Config.Labels \"orca.mount_recover.no-restore\"}}";

/// Parse [`DOCKER_STATE_FORMAT`] output; `None` when it is not that shape.
fn parse_docker_state(raw: &str) -> Option<ConsumerState> {
    let mut f = raw.trim_end_matches(['\r', '\n']).split('\t');
    let status = f.next().filter(|s| !s.is_empty())?;
    let finished_at = f.next().filter(|s| !s.is_empty())?;
    let no_restore = f.next().is_some_and(|l| l.trim() == "true");
    Some(match status {
        "exited" => ConsumerState::Stopped {
            finished_at: finished_at.to_string(),
            no_restore,
        },
        "running" | "paused" | "restarting" | "removing" => ConsumerState::Running,
        other => ConsumerState::Unrestorable {
            status: other.to_string(),
        },
    })
}

/// Parse the `id\tname\tsource\tdestination` lines emitted by
/// [`DOCKER_BIND_FORMAT`], keeping only binds whose host source falls under a
/// watched prefix. Pure so it's testable without Docker.
fn parse_docker_binds(raw: &str, watch: &[String]) -> Vec<ConsumerBind> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(id), Some(name), Some(source), Some(dest)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !path_under_watch(source, watch) {
            continue;
        }
        out.push(ConsumerBind {
            container_id: id.to_string(),
            // docker prefixes names with '/'; strip it for reporting.
            container_name: name.trim_start_matches('/').to_string(),
            host_source: source.to_string(),
            container_target: dest.to_string(),
        });
    }
    out
}

/// One Proxmox guest row from `pct list` (LXC container). `status` is the raw
/// `pct` state string (`running` / `stopped`); `name` is the guest hostname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PctGuest {
    pub vmid: String,
    pub status: String,
    pub name: String,
}

/// Production [`ContainerRuntime`] for Proxmox LXC guests, shelling `pct`.
/// Mirrors [`DockerCli`]: every `pct` shell-out is confined here. Unlike Docker,
/// a Proxmox guest can be STOPPED yet still declare a managed bind in its config,
/// so this runtime also implements [`ContainerRuntime::start_gated`].
pub struct PctCli;

/// `pct list` → typed guest rows. A `pct` failure is a hard `Err` (the caller
/// records it), mirroring `DockerCli::binds_under`.
async fn pct_list() -> Result<Vec<PctGuest>, RecoverError> {
    let stdout = Command::new("pct")
        .arg("list")
        .run_checked()
        .await
        .map_err(|e| RecoverError::ToolFailed {
            tool: "pct list",
            code: e.code,
            stderr: e.stderr,
        })?;
    Ok(parse_pct_list(&String::from_utf8_lossy(&stdout)))
}

/// `pct config <vmid>` → raw config text.
async fn pct_config(vmid: &str) -> Result<String, RecoverError> {
    let stdout = Command::new("pct")
        .arg("config")
        .arg(vmid)
        .run_checked()
        .await
        .map_err(|e| RecoverError::ToolFailed {
            tool: "pct config",
            code: e.code,
            stderr: e.stderr,
        })?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// `pct start <vmid>`.
async fn pct_start(vmid: &str) -> Result<(), RecoverError> {
    Command::new("pct")
        .arg("start")
        .arg(vmid)
        .run_checked()
        .await
        .map(|_stdout| ())
        .map_err(|e| RecoverError::ToolFailed {
            tool: "pct start",
            code: e.code,
            stderr: e.stderr,
        })
}

#[orca_async]
impl ContainerRuntime for PctCli {
    async fn binds_under(&self, watch: &[String]) -> Result<Vec<ConsumerBind>, RecoverError> {
        // Only RUNNING guests can be `pct exec`-probed; stopped guests are the
        // start_gated path. Collect the managed binds of each running guest.
        let mut out = Vec::new();
        for g in pct_list().await?.iter().filter(|g| g.status == "running") {
            let cfg = pct_config(&g.vmid).await?;
            out.extend(parse_pct_config_binds(&g.vmid, &g.name, &cfg, watch));
        }
        Ok(out)
    }

    async fn probe_path(
        &self,
        id: &str,
        path: &str,
        timeout: Duration,
    ) -> Result<ConsumerProbe, RecoverError> {
        let fut = Command::new("pct")
            .arg("exec")
            .arg(id)
            .arg("--")
            .arg("stat")
            .arg("--")
            .arg(path)
            .output();
        match crate::time::timeout(timeout, fut).await {
            // In-guest `stat` hung past the budget → stale (same rule as the host
            // probe's timeout→stale).
            None => Ok(ConsumerProbe::Stale),
            Some(Err(e)) => Err(RecoverError::Io(e)),
            Some(Ok(out)) if out.status.success => Ok(ConsumerProbe::Ok),
            Some(Ok(out)) => {
                if classify_stat_failure(&String::from_utf8_lossy(&out.stderr)) == "stale" {
                    Ok(ConsumerProbe::Stale)
                } else {
                    Err(RecoverError::Io(std::io::Error::other(
                        String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    )))
                }
            }
        }
    }

    async fn restart(&self, id: &str) -> Result<(), RecoverError> {
        Command::new("pct")
            .arg("reboot")
            .arg(id)
            .run_checked()
            .await
            .map(|_stdout| ())
            .map_err(|e| RecoverError::ToolFailed {
                tool: "pct reboot",
                code: e.code,
                stderr: e.stderr,
            })
    }

    async fn start_gated(
        &self,
        watch: &[String],
        host_healthy: &(dyn Fn(&str) -> bool + Sync),
    ) -> Vec<String> {
        let guests = match pct_list().await {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut started = Vec::new();
        for g in guests.iter().filter(|g| g.status == "stopped") {
            let Ok(cfg) = pct_config(&g.vmid).await else {
                continue;
            };
            let binds = parse_pct_config_binds(&g.vmid, &g.name, &cfg, watch);
            // Start only when a managed bind's covering host mount is healthy —
            // never start a guest against a stale/absent mount.
            if binds.iter().any(|b| host_healthy(&b.host_source))
                && pct_start(&g.vmid).await.is_ok()
            {
                started.push(g.name.clone());
            }
        }
        started
    }
}

/// Parse `pct list` output into guest rows. Columns are `VMID Status [Lock]
/// Name` with a variable-width, sometimes-empty `Lock`; `Name` is always last.
/// Pure so it's testable without `pct`.
fn parse_pct_list(raw: &str) -> Vec<PctGuest> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("VMID") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Need at least vmid + status + name; take vmid/status from the front and
        // name from the end so a present-or-absent Lock column doesn't shift it.
        let (Some(vmid), Some(status), Some(name)) = (fields.first(), fields.get(1), fields.last())
        else {
            continue;
        };
        if !vmid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        out.push(PctGuest {
            vmid: vmid.to_string(),
            status: status.to_string(),
            name: name.to_string(),
        });
    }
    out
}

/// Parse the `mpN:` bind entries from a `pct config <vmid>` dump, keeping only
/// binds whose host source falls under a watched prefix. Proxmox renders a
/// mountpoint as `mpN: <volume-or-hostpath>,mp=<container_path>[,opt...]`. A
/// **bind** of a host directory has an absolute path as its first token; a
/// storage-volume mount (e.g. `local-lvm:vm-113-disk-1`) does not and is
/// skipped. Pure so it's testable without `pct`.
fn parse_pct_config_binds(
    vmid: &str,
    name: &str,
    config_raw: &str,
    watch: &[String],
) -> Vec<ConsumerBind> {
    let mut out = Vec::new();
    for line in config_raw.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        // mpN: keys are the additional mountpoints; rootfs is never a network bind.
        let key = key.trim();
        if !(key.starts_with("mp") && key.len() > 2 && key[2..].chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let mut parts = value.trim().split(',');
        let Some(source) = parts.next().map(str::trim) else {
            continue;
        };
        // Host bind, not a storage volume: absolute path only.
        if !source.starts_with('/') {
            continue;
        }
        let Some(target) = parts
            .find_map(|p| p.trim().strip_prefix("mp="))
            .map(str::trim)
        else {
            continue;
        };
        if !path_under_watch(source, watch) {
            continue;
        }
        out.push(ConsumerBind {
            container_id: vmid.to_string(),
            container_name: name.to_string(),
            host_source: source.to_string(),
            container_target: target.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_under_watch_prefix_boundary() {
        let watch = vec!["/mnt/data".to_string()];
        assert!(path_under_watch("/mnt/data", &watch));
        assert!(path_under_watch("/mnt/data/media", &watch));
        assert!(!path_under_watch("/mnt/database", &watch));
        assert!(!path_under_watch("/mnt/other", &watch));
        // Empty watch matches everything.
        assert!(path_under_watch("/anything", &[]));
    }

    #[test]
    fn classify_stat_failure_detects_estale() {
        assert_eq!(classify_stat_failure("Stale file handle"), "stale");
        assert_eq!(
            classify_stat_failure("stat: ... Stale file HANDLE"),
            "stale"
        );
        assert_eq!(
            classify_stat_failure("No such file or directory"),
            "error: No such file or directory"
        );
    }

    #[test]
    fn classify_stat_failure_detects_cifs_flap_signatures() {
        // The CIFS/SMB errno strings a wedged bind emits from an in-container
        // `stat` — every one must classify stale so the restart recovery fires.
        for stderr in [
            "stat: cannot statx '/data': Bad file descriptor",
            "stat: cannot statx '/data': Host is down",
            "stat: cannot statx '/data': Input/output error",
            "stat: cannot statx '/data': Transport endpoint is not connected",
            "stat: cannot statx '/data': Not connected",
            "stat: cannot statx '/data': Resource temporarily unavailable",
        ] {
            assert_eq!(classify_stat_failure(stderr), "stale", "for: {stderr}");
        }
        assert_eq!(classify_stat_failure("BAD FILE DESCRIPTOR"), "stale");
        // A plain permission error is NOT a flap and must not trigger a restart.
        assert!(classify_stat_failure("Permission denied").starts_with("error:"));
    }

    #[test]
    fn parse_docker_binds_filters_by_watch() {
        let raw = "abc\t/media-server\t/mnt/data/media\t/data\n\
                   def\t/other\t/opt/appdata\t/config\n\
                   ghi\t/backups-job\t/mnt/backups/x\t/b\n";
        let watch = vec!["/mnt/data".to_string(), "/mnt/backups".to_string()];
        let binds = parse_docker_binds(raw, &watch);
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].container_name, "media-server");
        assert_eq!(binds[0].host_source, "/mnt/data/media");
        assert_eq!(binds[0].container_target, "/data");
        assert_eq!(binds[1].container_name, "backups-job");
    }

    #[test]
    fn parse_pct_list_handles_optional_lock_column() {
        let raw = "VMID       Status     Lock         Name\n\
                   110        running                 mimir\n\
                   113        stopped                 jellyfin\n\
                   200        running    backup       db\n\
                   notanid    running                 skip\n";
        let guests = parse_pct_list(raw);
        assert_eq!(guests.len(), 3);
        assert_eq!(guests[0].vmid, "110");
        assert_eq!(guests[0].status, "running");
        assert_eq!(guests[0].name, "mimir");
        // Lock column present must not shift the name.
        assert_eq!(guests[2].vmid, "200");
        assert_eq!(guests[2].name, "db");
    }

    #[test]
    fn parse_pct_config_binds_takes_host_binds_only() {
        let cfg = "arch: amd64\n\
                   mp0: /mnt/data,mp=/mnt/data,ro=1\n\
                   mp1: local-lvm:vm-113-disk-1,mp=/scratch\n\
                   mp2: /mnt/backups/jellyfin,mp=/mnt/backups\n\
                   rootfs: local-lvm:vm-113-disk-0,size=8G\n";
        let watch = vec!["/mnt/data".to_string(), "/mnt/backups".to_string()];
        let binds = parse_pct_config_binds("113", "jellyfin", cfg, &watch);
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].host_source, "/mnt/data");
        assert_eq!(binds[0].container_target, "/mnt/data");
        assert_eq!(binds[1].host_source, "/mnt/backups/jellyfin");
        assert_eq!(binds[1].container_target, "/mnt/backups");
    }

    // ── recover_stale_consumers with a fake runtime ─────────────────────────

    struct FakeRuntime {
        binds: Vec<ConsumerBind>,
        // container_target -> probe results, popped front-to-back per call.
        probes: std::sync::Mutex<std::collections::HashMap<String, Vec<ConsumerProbe>>>,
        restarted: std::sync::Mutex<Vec<String>>,
    }

    #[orca_async]
    impl ContainerRuntime for FakeRuntime {
        async fn binds_under(&self, _watch: &[String]) -> Result<Vec<ConsumerBind>, RecoverError> {
            Ok(self.binds.clone())
        }
        async fn probe_path(
            &self,
            _id: &str,
            path: &str,
            _timeout: Duration,
        ) -> Result<ConsumerProbe, RecoverError> {
            let mut p = self.probes.lock().unwrap();
            let seq = p.get_mut(path).expect("probe seq for path");
            Ok(seq.remove(0))
        }
        async fn restart(&self, id: &str) -> Result<(), RecoverError> {
            self.restarted.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    fn bind(id: &str, target: &str) -> ConsumerBind {
        ConsumerBind {
            container_id: id.to_string(),
            container_name: id.to_string(),
            host_source: "/mnt/data".to_string(),
            container_target: target.to_string(),
        }
    }

    #[tokio::test]
    async fn stale_consumer_restarted_when_host_healthy() {
        let mut probes = std::collections::HashMap::new();
        // First probe stale, post-restart probe ok.
        probes.insert(
            "/data".to_string(),
            vec![ConsumerProbe::Stale, ConsumerProbe::Ok],
        );
        let rt = FakeRuntime {
            binds: vec![bind("c1", "/data")],
            probes: std::sync::Mutex::new(probes),
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        let res = recover_stale_consumers(
            &rt,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| true,
        )
        .await;
        assert_eq!(res.recovered, vec!["c1"]);
        assert!(res.still_stale.is_empty());
        assert_eq!(rt.restarted.lock().unwrap().as_slice(), ["c1"]);
    }

    #[tokio::test]
    async fn stale_consumer_skipped_when_host_stale() {
        let mut probes = std::collections::HashMap::new();
        probes.insert("/data".to_string(), vec![ConsumerProbe::Stale]);
        let rt = FakeRuntime {
            binds: vec![bind("c1", "/data")],
            probes: std::sync::Mutex::new(probes),
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        // host_healthy = false → must NOT restart (outage guard).
        let res = recover_stale_consumers(
            &rt,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| false,
        )
        .await;
        assert_eq!(res.skipped_host_stale, vec!["c1"]);
        assert!(rt.restarted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn healthy_consumer_not_restarted() {
        let mut probes = std::collections::HashMap::new();
        probes.insert("/data".to_string(), vec![ConsumerProbe::Ok]);
        let rt = FakeRuntime {
            binds: vec![bind("c1", "/data")],
            probes: std::sync::Mutex::new(probes),
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        let res = recover_stale_consumers(
            &rt,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| true,
        )
        .await;
        assert_eq!(res.healthy, vec!["c1"]);
        assert!(rt.restarted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_binds_reports_no_consumers() {
        let rt = FakeRuntime {
            binds: vec![],
            probes: std::sync::Mutex::new(std::collections::HashMap::new()),
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        let res = recover_stale_consumers(
            &rt,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| true,
        )
        .await;
        assert!(res.no_consumers_found);
    }

    // ── stopped-consumer ledger ─────────────────────────────────────────────

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Runtime whose first `restart` of each `fail_start` container leaves it
    /// stopped, as a docker restart does when the bind source has vanished.
    #[derive(Default)]
    struct StatefulRuntime {
        binds: Vec<ConsumerBind>,
        fail_start: Mutex<Vec<String>>,
        start_errors: bool,
        state_answer: Option<Result<Option<ConsumerState>, ()>>,
        states: Mutex<HashMap<String, ConsumerState>>,
        started: Arc<Mutex<Vec<String>>>,
    }

    #[orca_async]
    impl ContainerRuntime for StatefulRuntime {
        async fn binds_under(&self, _watch: &[String]) -> Result<Vec<ConsumerBind>, RecoverError> {
            let states = self.states.lock().unwrap();
            Ok(self
                .binds
                .iter()
                .filter(|b| states.get(&b.container_id) == Some(&ConsumerState::Running))
                .cloned()
                .collect())
        }
        async fn probe_path(
            &self,
            _id: &str,
            _path: &str,
            _timeout: Duration,
        ) -> Result<ConsumerProbe, RecoverError> {
            Ok(ConsumerProbe::Stale)
        }
        async fn restart(&self, id: &str) -> Result<(), RecoverError> {
            let mut fail = self.fail_start.lock().unwrap();
            if let Some(i) = fail.iter().position(|f| f == id) {
                fail.remove(i);
                self.set(id, stopped("t1"));
                return Err(RecoverError::Io(std::io::Error::other(
                    "bind source missing",
                )));
            }
            Ok(())
        }
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn state(&self, id: &str) -> Result<Option<ConsumerState>, RecoverError> {
            match &self.state_answer {
                Some(Ok(a)) => return Ok(a.clone()),
                Some(Err(())) => return Err(RecoverError::Unsupported("state")),
                None => {}
            }
            Ok(Some(
                self.states
                    .lock()
                    .unwrap()
                    .get(id)
                    .cloned()
                    .unwrap_or(ConsumerState::Gone),
            ))
        }
        async fn start(&self, id: &str) -> Result<(), RecoverError> {
            self.started.lock().unwrap().push(id.to_string());
            if self.start_errors {
                return Err(RecoverError::Io(std::io::Error::other("no such source")));
            }
            self.set(id, ConsumerState::Running);
            Ok(())
        }
    }

    impl StatefulRuntime {
        fn set(&self, id: &str, st: ConsumerState) {
            self.states.lock().unwrap().insert(id.to_string(), st);
        }
    }

    fn stopped(at: &str) -> ConsumerState {
        ConsumerState::Stopped {
            finished_at: at.to_string(),
            no_restore: false,
        }
    }

    type Started = Arc<Mutex<Vec<String>>>;

    fn boxed(rt: StatefulRuntime) -> (Vec<Box<dyn ContainerRuntime>>, Started) {
        let started = rt.started.clone();
        (vec![Box::new(rt)], started)
    }

    fn two_consumers_one_breaks() -> (Vec<Box<dyn ContainerRuntime>>, Started) {
        let rt = StatefulRuntime {
            binds: vec![bind("c1", "/data"), bind("c2", "/data")],
            fail_start: Mutex::new(vec!["c1".to_string()]),
            ..Default::default()
        };
        rt.set("c1", ConsumerState::Running);
        rt.set("c2", ConsumerState::Running);
        boxed(rt)
    }

    const OWNER: &str = "test";

    async fn sweep(
        rts: &[Box<dyn ContainerRuntime>],
        healthy: bool,
        home: &Path,
    ) -> ConsumerRecoverResult {
        sweep_as(OWNER, rts, Some(healthy), home).await
    }

    async fn sweep_as(
        owner: &str,
        rts: &[Box<dyn ContainerRuntime>],
        healthy: Option<bool>,
        home: &Path,
    ) -> ConsumerRecoverResult {
        recover_consumers_for_owner_at(
            owner,
            rts,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| healthy,
            Some(home),
        )
        .await
    }

    fn load(home: &Path, owner: &str) -> Vec<StoppedConsumer> {
        let mut errors = Vec::new();
        let l = load_stopped_at(&stopped_ledger_file(home, owner), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        l
    }

    #[tokio::test]
    async fn restart_that_leaves_consumer_stopped_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let (rts, _) = two_consumers_one_breaks();
        sweep(&rts, true, path).await;
        let ledger = load(path, OWNER);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].container_id, "c1");
        assert_eq!(ledger[0].finished_at, "t1");
    }

    #[tokio::test]
    async fn recovery_restarts_exactly_recorded_after_confirmed_health() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let (rts, started) = two_consumers_one_breaks();
        sweep(&rts, true, path).await;
        for _ in 1..RESTORE_CONFIRM_SWEEPS {
            sweep(&rts, true, path).await;
            assert!(
                started.lock().unwrap().is_empty(),
                "not before confirm streak"
            );
        }
        let res = sweep(&rts, true, path).await;
        assert_eq!(started.lock().unwrap().as_slice(), ["c1"]);
        assert!(res.recovered.contains(&"restored:c1".to_string()));
        assert!(load(path, OWNER).is_empty());
    }

    #[tokio::test]
    async fn flapping_mount_never_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let (rts, started) = two_consumers_one_breaks();
        sweep(&rts, true, path).await;
        for i in 0..(RESTORE_CONFIRM_SWEEPS * 4) {
            sweep(&rts, i % 2 == 0, path).await;
        }
        assert!(started.lock().unwrap().is_empty());
        assert_eq!(load(path, OWNER).len(), 1);
    }

    #[tokio::test]
    async fn operator_started_restopped_or_removed_consumers_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let rt = StatefulRuntime::default();
        let mk = |id: &str| StoppedConsumer {
            runtime: "fake".into(),
            container_id: id.into(),
            container_name: id.into(),
            host_source: "/mnt/data".into(),
            finished_at: "t1".into(),
            healthy_sweeps: RESTORE_CONFIRM_SWEEPS,
            start_failures: 0,
        };
        rt.set("running", ConsumerState::Running);
        rt.set("restopped", stopped("t2"));
        save_stopped_at(
            &stopped_ledger_file(path, OWNER),
            &[mk("running"), mk("restopped"), mk("removed")],
        )
        .unwrap();
        let (rts, started) = boxed(rt);
        sweep(&rts, true, path).await;
        assert!(started.lock().unwrap().is_empty());
        assert!(load(path, OWNER).is_empty());
    }

    #[tokio::test]
    async fn record_survives_restart_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let (first, _) = two_consumers_one_breaks();
        sweep(&first, true, path).await;
        sweep(&first, true, path).await;
        assert_eq!(load(path, OWNER)[0].healthy_sweeps, 1);
        drop(first);

        // A fresh process: only the on-disk record knows c1 was ours.
        let rt = StatefulRuntime::default();
        rt.set("c1", stopped("t1"));
        let (rts, started) = boxed(rt);
        for _ in 1..RESTORE_CONFIRM_SWEEPS {
            sweep(&rts, true, path).await;
        }
        assert_eq!(started.lock().unwrap().as_slice(), ["c1"]);
    }

    fn recorded(id: &str) -> StoppedConsumer {
        StoppedConsumer {
            runtime: "fake".into(),
            container_id: id.into(),
            container_name: id.into(),
            host_source: "/mnt/data".into(),
            finished_at: "t1".into(),
            healthy_sweeps: RESTORE_CONFIRM_SWEEPS,
            start_failures: 0,
        }
    }

    fn seed(home: &Path, owner: &str, entries: &[StoppedConsumer]) {
        save_stopped_at(&stopped_ledger_file(home, owner), entries).unwrap();
    }

    #[tokio::test]
    async fn two_owners_sharing_a_home_do_not_interfere() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        seed(home, "nfs", &[recorded("c1")]);
        seed(home, "smb", &[recorded("c2")]);
        let rt = StatefulRuntime::default();
        rt.set("c1", stopped("t1"));
        rt.set("c2", stopped("t1"));
        let (rts, started) = boxed(rt);

        // smb's sources are not nfs's: None leaves smb's ledger untouched.
        sweep_as("smb", &rts, None, home).await;
        sweep_as("nfs", &rts, Some(true), home).await;
        assert_eq!(started.lock().unwrap().as_slice(), ["c1"]);
        assert!(load(home, "nfs").is_empty());
        assert_eq!(load(home, "smb"), vec![recorded("c2")]);
    }

    #[tokio::test]
    async fn not_mine_keeps_healthy_streak() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = recorded("c1");
        e.healthy_sweeps = 1;
        seed(dir.path(), OWNER, &[e.clone()]);
        let rt = StatefulRuntime::default();
        rt.set("c1", stopped("t1"));
        let (rts, started) = boxed(rt);
        sweep_as(OWNER, &rts, None, dir.path()).await;
        assert!(started.lock().unwrap().is_empty());
        assert_eq!(load(dir.path(), OWNER), vec![e]);
    }

    #[tokio::test]
    async fn state_unknown_or_error_keeps_record() {
        for answer in [Ok(None), Err(())] {
            let dir = tempfile::tempdir().unwrap();
            seed(dir.path(), OWNER, &[recorded("c1")]);
            let is_err = answer.is_err();
            let rt = StatefulRuntime {
                state_answer: Some(answer),
                ..Default::default()
            };
            let (rts, started) = boxed(rt);
            let res = sweep(&rts, true, dir.path()).await;
            assert!(started.lock().unwrap().is_empty());
            assert_eq!(load(dir.path(), OWNER), vec![recorded("c1")]);
            assert_eq!(res.errors.iter().any(|e| e.starts_with("state c1")), is_err);
        }
    }

    #[tokio::test]
    async fn failing_start_is_retried_then_given_up() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), OWNER, &[recorded("c1")]);
        let rt = StatefulRuntime {
            start_errors: true,
            ..Default::default()
        };
        rt.set("c1", stopped("t1"));
        let (rts, started) = boxed(rt);
        for n in 1..MAX_START_FAILURES {
            sweep(&rts, true, dir.path()).await;
            assert_eq!(load(dir.path(), OWNER)[0].start_failures, n);
        }
        let res = sweep(&rts, true, dir.path()).await;
        assert!(load(dir.path(), OWNER).is_empty());
        assert_eq!(
            res.errors
                .iter()
                .filter(|e| e.starts_with("giving up"))
                .count(),
            1
        );
        let res = sweep(&rts, true, dir.path()).await;
        assert!(res.errors.is_empty());
        assert_eq!(started.lock().unwrap().len() as u32, MAX_START_FAILURES);
    }

    #[tokio::test]
    async fn opt_out_label_and_unrestorable_are_dropped_unstarted() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path(), OWNER, &[recorded("optout"), recorded("dead")]);
        let rt = StatefulRuntime::default();
        rt.set(
            "optout",
            ConsumerState::Stopped {
                finished_at: "t1".into(),
                no_restore: true,
            },
        );
        rt.set(
            "dead",
            ConsumerState::Unrestorable {
                status: "dead".into(),
            },
        );
        let (rts, started) = boxed(rt);
        let res = sweep(&rts, true, dir.path()).await;
        assert!(started.lock().unwrap().is_empty());
        assert!(load(dir.path(), OWNER).is_empty());
        assert_eq!(res.errors.len(), 2, "{:?}", res.errors);
    }

    #[tokio::test]
    async fn corrupt_ledger_is_reported_and_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = stopped_ledger_file(dir.path(), OWNER);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let (rts, _) = boxed(StatefulRuntime::default());
        let res = sweep(&rts, true, dir.path()).await;
        assert!(res.errors.iter().any(|e| e.starts_with("corrupt")));
        assert!(!path.exists());
        let aside = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt."));
        assert!(aside);
    }

    #[tokio::test]
    async fn no_home_is_reported() {
        let (rts, _) = boxed(StatefulRuntime::default());
        let res = recover_consumers_for_owner_at(
            OWNER,
            &rts,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| Some(true),
            None,
        )
        .await;
        assert!(res.errors.iter().any(|e| e.contains("no orca home")));
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn deprecated_multi_wrapper_still_runs() {
        let (rts, _) = boxed(StatefulRuntime::default());
        let res = recover_consumers_multi(
            &rts,
            &["/mnt/data".to_string()],
            Duration::from_secs(1),
            |_| true,
        )
        .await;
        assert!(!res.errors.iter().any(|e| e.contains("invalid owner")));
    }

    #[tokio::test]
    async fn invalid_owner_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (rts, _) = boxed(StatefulRuntime::default());
        for bad in ["", "NFS", "a/b", &"x".repeat(33)] {
            let res = sweep_as(bad, &rts, Some(true), dir.path()).await;
            assert!(
                res.errors.iter().any(|e| e.contains("invalid owner")),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn held_lock_skips_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let held = lock_ledger(&stopped_ledger_file(dir.path(), OWNER))
            .unwrap()
            .unwrap();
        let (rts, _) = boxed(StatefulRuntime::default());
        let res = sweep(&rts, true, dir.path()).await;
        assert!(res.errors.iter().any(|e| e.contains(&format!(
            "mount recovery sweep already in progress for {OWNER}"
        ))));
        drop(held);
    }

    #[tokio::test]
    async fn default_start_is_unsupported() {
        let rt = FakeRuntime {
            binds: vec![],
            probes: std::sync::Mutex::new(HashMap::new()),
            restarted: std::sync::Mutex::new(Vec::new()),
        };
        assert!(matches!(
            rt.start("x").await,
            Err(RecoverError::Unsupported("start"))
        ));
    }

    #[test]
    fn parse_docker_state_shapes() {
        assert_eq!(
            parse_docker_state("running\t0001-01-01T00:00:00Z\t\n"),
            Some(ConsumerState::Running)
        );
        assert_eq!(
            parse_docker_state("exited\t2026-10-12T01:02:03Z\t\n"),
            Some(stopped("2026-10-12T01:02:03Z"))
        );
        assert_eq!(
            parse_docker_state("exited\tt1\ttrue\n"),
            Some(ConsumerState::Stopped {
                finished_at: "t1".into(),
                no_restore: true,
            })
        );
        assert_eq!(
            parse_docker_state("dead\tt1\t\n"),
            Some(ConsumerState::Unrestorable {
                status: "dead".into()
            })
        );
        assert_eq!(parse_docker_state(""), None);
        assert_eq!(parse_docker_state("exited\n"), None);
    }
}
