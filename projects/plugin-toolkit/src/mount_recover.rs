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
//! (guest) half. A backend calls [`recover_consumers_multi`] with its detected
//! runtimes and a `host_healthy` closure computed from its own mount table.

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
                match runtime.restart(&bind.container_id).await {
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
pub async fn recover_consumers_multi<F>(
    runtimes: &[Box<dyn ContainerRuntime>],
    watch: &[String],
    health_timeout: Duration,
    host_healthy: F,
) -> ConsumerRecoverResult
where
    F: Fn(&str) -> bool + Sync,
{
    let mut merged = ConsumerRecoverResult {
        no_consumers_found: true,
        ..Default::default()
    };
    for rt in runtimes {
        let c = recover_stale_consumers(rt.as_ref(), watch, health_timeout, &host_healthy).await;
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
        let started = rt.start_gated(watch, &host_healthy).await;
        if !started.is_empty() {
            merged.no_consumers_found = false;
            merged
                .recovered
                .extend(started.into_iter().map(|n| format!("started:{n}")));
        }
    }
    merged
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
}
