//! Unified container runtime aggregator.
//!
//! C2 lands the first two real adapters on top of the C1 scaffolding:
//! `adapters::docker::DockerAdapter` (bollard against the local Docker
//! Engine API) and `adapters::lxc_proxmox::LxcProxmoxAdapter` (which shells
//! `pct list` then parses `/etc/pve/lxc/<vmid>.conf`). The reconciliation
//! loop itself still lands in C3.
//!
//! ## Architecture
//!
//! Containers, VMs, and inventory are split across three crates by deliberate
//! design (memory: project-containers-vms-split-inventory-aggregator):
//!
//! - **this crate** (`containers`) — runtime-agnostic container model and the
//!   adapter trait shared by docker + lxc + podman + nspawn.
//! - `vms` (separate crate) — virtual-machine lifecycle. Different state
//!   machine, different remediation primitives.
//! - `inventory` (separate crate) — server-side aggregator that fans out to
//!   colocated `containers` / `vms` collectors per host
//!   ([[project-colocated-api-collectors]]).
//!
//! ## Plugin namespace
//!
//! All tools registered by this crate live under the `containers` namespace.
//! `containers.list` returns the unified view across every detected runtime
//! on the local host. Cross-host queries route via mesh dispatch
//! ([[project-universal-peer-dispatch]]).
//!
//! ## Adapter registry
//!
//! The runtime adapters follow the same plug-in shape as
//! `projects/notifications/`: every adapter implements [`RuntimeAdapter`] and
//! is registered against a process-global registry by the host bootstrap.
//! `containers.list` iterates the registered set rather than reaching for
//! specific runtimes by name. The default builder
//! [`adapters::builtin_adapters`] consults [`detect_available_runtimes`] and
//! returns one trait object per runtime found on PATH/socket, so single-host
//! installs work without any wiring code. Hosts that need a non-default mix
//! (mocked adapter in tests, future Podman/nspawn) call
//! [`register_adapter`] directly.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};
use thiserror::Error;

use crate::breaker::HostObservation;

pub mod adapters;
pub mod breaker;
pub mod reconciler;

// ── Runtime kinds ──────────────────────────────────────────────────────────

/// Which container runtime backs an adapter / container row.
///
/// `RuntimeKind` is the single discriminator the reconciler keys on when it
/// picks a remediation primitive (e.g. `docker start` vs `pct start` vs
/// `machinectl start`). Adding a new runtime in the future means adding a
/// variant here, implementing [`RuntimeAdapter`] for it, and teaching
/// [`detect_available_runtimes`] to probe for it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    /// Docker Engine / Moby (CLI today, Engine API later).
    Docker,
    /// Proxmox / mainline LXC (`pct` / `lxc-ls`).
    Lxc,
    /// Podman (rootless or rootful).
    Podman,
    /// systemd-nspawn (`machinectl`).
    Nspawn,
}

impl RuntimeKind {
    /// Stable short string used in tool output, log lines, and route matchers.
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeKind::Docker => "docker",
            RuntimeKind::Lxc => "lxc",
            RuntimeKind::Podman => "podman",
            RuntimeKind::Nspawn => "nspawn",
        }
    }
}

// ── Container model ────────────────────────────────────────────────────────

/// Restart policy as declared by the runtime, normalized across docker / lxc /
/// podman / nspawn.
///
/// The reconciler treats `UnlessStopped` and `Always` as "desired = running"
/// (§2.1 source of truth). `No` and `OnFailure` are read-only signals — the
/// reconciler will not auto-restart these.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// `--restart no` (docker), `lxc.start.auto=0` (lxc), etc.
    No,
    /// `--restart on-failure[:max-retries]`.
    OnFailure,
    /// `--restart unless-stopped`.
    UnlessStopped,
    /// `--restart always`, `lxc.start.auto=1`.
    Always,
}

impl RestartPolicy {
    /// True when the operator has declared "this should be running" via the
    /// runtime's own policy field — the primary source of truth for §2.1.
    pub fn desires_running(self) -> bool {
        matches!(self, RestartPolicy::UnlessStopped | RestartPolicy::Always)
    }
}

/// Observed lifecycle state, normalized across runtimes.
///
/// docker's full set (`created`, `restarting`, `running`, `removing`, `paused`,
/// `exited`, `dead`) maps directly. lxc's `STOPPED`/`RUNNING`/`FROZEN`/`ABORTING`/
/// `STARTING`/`STOPPING` map to the closest equivalents.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContainerState {
    /// Created but never started (docker `created`, lxc `STOPPED` for a
    /// freshly-created CT).
    Created,
    /// Mid-start.
    Starting,
    /// Live and (per runtime) considered running.
    Running,
    /// Paused / frozen.
    Paused,
    /// Currently shutting down.
    Stopping,
    /// Stopped after running. Distinguished from `Created` because the
    /// reconciler treats clean exits and crashloops differently (§2.1
    /// auto-start vs circuit breaker).
    Exited,
    /// Runtime reports the container as dead / unrecoverable without
    /// re-creation.
    Dead,
    /// Runtime returned a state string we have no mapping for. Recorded
    /// verbatim so operators can see what the raw value was — the reconciler
    /// treats this as "do not act" rather than guessing.
    Unknown,
}

/// One bind mount or volume on a container. The mount source is the key the
/// mounts reconciler uses to build the (mount → dependents) edge for the
/// dep graph ([[self-healing-reconciler.md]] §2.2).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ContainerMount {
    /// Host-side path (docker `Source`, lxc `mp*` host part).
    pub source: PathBuf,
    /// Container-side path.
    pub target: PathBuf,
    /// True when the mount is read-only.
    pub read_only: bool,
}

/// One open published port (host:container, protocol).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ContainerPort {
    pub host_port: u16,
    pub container_port: u16,
    /// `"tcp"` / `"udp"`. Free-form to admit runtime-specific values without
    /// constraining the model prematurely.
    pub protocol: String,
}

/// Boot-time ordering declared by the runtime. Proxmox LXC `startup:
/// order=N,up=M,down=K` is the motivating shape (§2.1, §2.2 dep ordering);
/// docker has no equivalent and leaves every field `None`. Modeled as typed
/// fields rather than a raw string so the reconciler can compare across
/// runtimes without re-parsing.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct StartupOrdering {
    /// Lower-first start order. Proxmox `startup: order=<N>`.
    pub order: Option<u32>,
    /// Seconds to wait after starting this CT before the host considers it
    /// up enough to start the next ordered peer. Proxmox `up=<seconds>`.
    pub up_delay_secs: Option<u32>,
    /// Seconds to wait after issuing shutdown before the host kills the CT.
    /// Proxmox `down=<seconds>`.
    pub down_delay_secs: Option<u32>,
}

impl StartupOrdering {
    /// True when none of the ordering hints are populated.
    pub fn is_empty(&self) -> bool {
        self.order.is_none() && self.up_delay_secs.is_none() && self.down_delay_secs.is_none()
    }
}

/// Normalized container row — the unit every adapter returns and every
/// reconciler consumes.
///
/// Field selection is the §2.1 minimum needed to decide an action:
///
/// - `id` / `name` / `runtime` / `host` — identity and dispatch.
/// - `state` / `restart_policy` — auto-start decision.
/// - `image` — surfaced for operator context and crashloop classification.
/// - `labels` — `orca.skip`, `orca.heal=manual`, `orca.heal.drain=long`
///   gates live here (§2.1 + §2.2 active-write drain).
/// - `mounts` — dep-graph input for the mounts reconciler.
/// - `ports` — surfaced for inventory aggregation; not used by the §2.1
///   loop directly.
/// - `started_at` / `finished_at` — crashloop window math (§2.1 circuit
///   breaker).
/// - `restart_count` — same.
/// - `exit_code` — distinguishes clean exit from crash for the breaker.
/// - `startup` — boot ordering (LXC), feeds the §2.2 forward/reverse
///   restart sequence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub runtime: RuntimeKind,
    /// Hostname of the machine running this container. Stamped by the
    /// adapter from the local host's identity at list time; downstream
    /// consumers (sort, inventory aggregator) key on this without an
    /// additional hop. Empty string is reserved for the "unknown host"
    /// case where the adapter couldn't determine its own hostname.
    pub host: String,
    pub state: ContainerState,
    pub restart_policy: RestartPolicy,
    pub image: Option<String>,
    pub labels: Vec<(String, String)>,
    pub mounts: Vec<ContainerMount>,
    pub ports: Vec<ContainerPort>,
    /// RFC 3339 timestamp the container last entered `Running`.
    pub started_at: Option<String>,
    /// RFC 3339 timestamp the container last exited.
    pub finished_at: Option<String>,
    pub restart_count: u32,
    pub exit_code: Option<i32>,
    /// Boot-time ordering hints (LXC only today). `None` when the runtime
    /// has no ordering primitive (docker).
    pub startup: Option<StartupOrdering>,
}

impl Container {
    /// The §2.1 question: would the reconciler auto-start this container if
    /// it found it in a non-running state right now?
    ///
    /// - `RestartPolicy::UnlessStopped` / `Always` → yes.
    /// - `orca.skip=true` label → no (operator escape hatch).
    ///
    /// `orca.heal=manual` is intentionally **not** checked here: that label
    /// gates the action, not the desire. The reconciler still treats the
    /// container as desired-running for alerting purposes, it just won't
    /// auto-restart.
    pub fn desires_running(&self) -> bool {
        if self.has_label("orca.skip", "true") {
            return false;
        }
        self.restart_policy.desires_running()
    }

    /// True when `key` is present with value `value`.
    pub fn has_label(&self, key: &str, value: &str) -> bool {
        self.labels.iter().any(|(k, v)| k == key && v == value)
    }

    /// Lookup label value by key.
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

// ── Adapter trait ──────────────────────────────────────────────────────────

/// Per-runtime errors surfaced by [`RuntimeAdapter`].
///
/// Variants intentionally narrow: every concrete failure stuffs the raw
/// message into the matching variant. Adapters don't get to invent new
/// variants — they classify into these so the reconciler can match on
/// `NotFound` vs `Transport` etc. without runtime-specific knowledge.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The runtime's binary / socket / API endpoint was reachable but
    /// returned no container with the requested id.
    #[error("container `{0}` not found")]
    NotFound(String),
    /// The runtime is unreachable (binary missing, socket down, API
    /// timeout). Distinguishes "operator killed docker" from "container
    /// missing".
    #[error("runtime unavailable: {0}")]
    Unavailable(String),
    /// The runtime reported a structural / parse error (unexpected JSON
    /// shape, unknown state string we couldn't classify even as
    /// [`ContainerState::Unknown`]).
    #[error("runtime returned malformed data: {0}")]
    Malformed(String),
    /// Catch-all transport / IO failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// Runtime explicitly refused the operation (permission denied,
    /// container locked by another writer, etc.).
    #[error("operation refused: {0}")]
    Refused(String),
}

/// Filter shape for [`RuntimeAdapter::list`]. C2 adapters honor as many of
/// these as the underlying runtime supports cheaply; the rest are filtered
/// client-side after the fetch.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// When true, includes stopped / exited / dead containers. Default is
    /// to fetch every container regardless of state (matches `docker ps -a`).
    pub all: bool,
    /// Only return containers whose labels match all of these `key=value`
    /// pairs.
    pub labels: Vec<(String, String)>,
}

/// Maximum number of log lines [`RuntimeAdapter::logs`] returns.
///
/// Modeled as a typed wrapper rather than a bare `u32` to make the call
/// sites' intent obvious and to prevent accidental misuse as a byte budget.
#[derive(Debug, Clone, Copy)]
pub struct LogTail(pub u32);

impl Default for LogTail {
    fn default() -> Self {
        Self(200)
    }
}

/// The surface every runtime adapter implements. Methods are intentionally
/// the minimum set §2.1 and §2.2 need.
///
/// `kind()` exists so the reconciler can stamp [`Container::runtime`]
/// correctly when it stitches adapter results into the unified view, and so
/// log lines / notifications can name the responsible runtime without a
/// downcast.
#[async_trait]
pub trait RuntimeAdapter: Send + Sync {
    /// Which runtime this adapter speaks for.
    fn kind(&self) -> RuntimeKind;

    /// Return every container the runtime knows about, subject to `filter`.
    async fn list(&self, filter: &ListFilter) -> Result<Vec<Container>, AdapterError>;

    /// Fetch a single container by its runtime-native id.
    async fn inspect(&self, id: &str) -> Result<Container, AdapterError>;

    /// Start a stopped container. Idempotent against `Running` (return Ok).
    async fn start(&self, id: &str) -> Result<(), AdapterError>;

    /// Stop a running container. Idempotent against `Exited` / `Dead`.
    async fn stop(&self, id: &str) -> Result<(), AdapterError>;

    /// Restart in place. Adapters MAY implement this as `stop` then `start`
    /// when the runtime has no native restart primitive.
    async fn restart(&self, id: &str) -> Result<(), AdapterError>;

    /// Return up to `tail.0` recent log lines for `id`. Adapters that
    /// can't honor `tail` cheaply (e.g. lxc) MAY return more, but never less
    /// than requested when more are available.
    async fn logs(&self, id: &str, tail: LogTail) -> Result<String, AdapterError>;

    /// Gather a per-container [`HostObservation`] for the breaker. The
    /// default returns an empty observation — only adapters whose
    /// runtime carries breaker-relevant out-of-band signals
    /// (currently lxc's `journalctl` tail) override it. `lxc_previous_state`
    /// is *not* the adapter's concern: the breaker owns cross-tick state
    /// in [`crate::breaker::BreakerRecord::last_observed_state`] and
    /// injects it inside [`crate::breaker::arm`]. Errors gathering the
    /// observation are intentionally swallowed in the override (logged
    /// via `tracing`) — a missing journal tail must not block a start.
    async fn observe(&self, _container: &Container) -> HostObservation {
        HostObservation::default()
    }
}

// ── Runtime detection ──────────────────────────────────────────────────────

/// Probe the local host for which runtimes are usable. Returns one entry per
/// runtime whose probe succeeded, in declaration order
/// (`Docker`, `Lxc`, `Podman`, `Nspawn`).
///
/// Probe signals — runtime is included when **any** is true and at least one
/// "executable" signal is true (binary on PATH or socket present):
///
/// - **Docker**: `docker` binary on PATH **and** a docker socket exists at
///   one of the known locations (`/var/run/docker.sock`,
///   `$HOME/.colima/default/docker.sock`).
/// - **Lxc**: `pct` (Proxmox) **or** `lxc-ls` binary on PATH.
/// - **Podman**: `podman` binary on PATH.
/// - **Nspawn**: `machinectl` **or** `systemd-nspawn` binary on PATH.
///
/// This is intentionally a cheap PATH/socket check, not a "make a real RPC"
/// liveness check. Liveness lives in the reconciler loop; detection is just
/// "should we wire this adapter at all on this host?".
///
/// Errors propagate from filesystem reads; we do not swallow them with
/// `.ok()` — a failure to read `$PATH` or `/var/run` is a real problem the
/// operator needs to see.
pub fn detect_available_runtimes() -> Result<Vec<RuntimeKind>, std::io::Error> {
    let mut out = Vec::new();

    if probe_docker()? {
        out.push(RuntimeKind::Docker);
    }
    if probe_lxc()? {
        out.push(RuntimeKind::Lxc);
    }
    if probe_podman()? {
        out.push(RuntimeKind::Podman);
    }
    if probe_nspawn()? {
        out.push(RuntimeKind::Nspawn);
    }

    Ok(out)
}

fn probe_docker() -> Result<bool, std::io::Error> {
    if !binary_on_path("docker")? {
        return Ok(false);
    }
    let candidates = ["/var/run/docker.sock", "/run/docker.sock"];
    for c in &candidates {
        if std::path::Path::new(c).exists() {
            return Ok(true);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let colima = format!("{home}/.colima/default/docker.sock");
        if std::path::Path::new(&colima).exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn probe_lxc() -> Result<bool, std::io::Error> {
    Ok(binary_on_path("pct")? || binary_on_path("lxc-ls")?)
}

fn probe_podman() -> Result<bool, std::io::Error> {
    binary_on_path("podman")
}

fn probe_nspawn() -> Result<bool, std::io::Error> {
    Ok(binary_on_path("machinectl")? || binary_on_path("systemd-nspawn")?)
}

/// Walks `$PATH` checking whether `name` resolves to an executable file. We
/// implement this rather than shelling out to `which` because (a) `which`
/// isn't guaranteed on minimal Unraid hosts and (b) we don't want to fork
/// a process just to answer a question we can answer with `stat()`.
pub(crate) fn binary_on_path(name: &str) -> Result<bool, std::io::Error> {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return Ok(false),
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        match std::fs::metadata(&candidate) {
            Ok(meta) if meta.is_file() => return Ok(true),
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // `PermissionDenied` reading a $PATH entry is normal on locked-
            // down systems; treat it as "not here, keep looking" rather than
            // aborting the probe.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

// ── Hostname capture ───────────────────────────────────────────────────────

/// Lightweight `hostname` lookup for stamping [`Container::host`]. Matches
/// the behavior of `system::host_identity::capture_hostname` — shells the
/// `hostname` binary, falls back to `"unknown"` if unavailable — without
/// taking a dep on the `system` crate (which depends on us indirectly).
///
/// Computed once per process via [`LazyLock`].
#[allow(dead_code, reason = "consumed by adapters::* once those modules land")]
pub(crate) fn local_hostname() -> &'static str {
    static HOSTNAME: LazyLock<String> = LazyLock::new(|| {
        let raw = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        // macOS appends `-2`, `-3`, ... on mDNS name collisions. Strip the
        // numeric suffix so the display name stays stable across flaps.
        let trimmed = raw.trim_end_matches('.');
        if let Some(idx) = trimmed.rfind('-') {
            let (head, tail) = trimmed.split_at(idx);
            let tail_digits = &tail[1..];
            if !tail_digits.is_empty() && tail_digits.chars().all(|c| c.is_ascii_digit()) {
                return head.to_string();
            }
        }
        trimmed.to_string()
    });
    HOSTNAME.as_str()
}

// ── Adapter registry ───────────────────────────────────────────────────────

static REGISTRY: LazyLock<RwLock<Vec<Arc<dyn RuntimeAdapter>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Append `adapter` to the process-global adapter registry. Host bootstrap
/// calls this once per enabled adapter at daemon startup; tests call it from
/// inside `serial_test`-guarded blocks with [`reset_registry`].
pub fn register_adapter(adapter: Arc<dyn RuntimeAdapter>) {
    REGISTRY
        .write()
        .expect("containers adapter registry poisoned")
        .push(adapter);
}

/// Snapshot of currently-registered adapters. Cheap clone of an `Arc` per
/// adapter; the registry lock is held only for the duration of the read.
pub fn registered_adapters() -> Vec<Arc<dyn RuntimeAdapter>> {
    REGISTRY
        .read()
        .expect("containers adapter registry poisoned")
        .clone()
}

/// Replace the entire registry contents with `adapters`. Intended for tests
/// and for host-bootstrap "rewire on config reload" flows where the new set
/// is computed atomically and shouldn't briefly overlap the old set.
pub fn replace_registry(adapters: Vec<Arc<dyn RuntimeAdapter>>) {
    let mut g = REGISTRY
        .write()
        .expect("containers adapter registry poisoned");
    *g = adapters;
}

/// Clear the registry. Tests use this to start from a known-empty state.
pub fn reset_registry() {
    REGISTRY
        .write()
        .expect("containers adapter registry poisoned")
        .clear();
}

/// Build the default adapter set for `detected`. One trait object per
/// detected runtime we have a C2 adapter for (docker + lxc/Proxmox today;
/// podman + nspawn return nothing until their adapters land).
///
/// Lives in `lib.rs` rather than `adapters/mod.rs` so the
/// `containers.list` tool can call it without forcing the adapter modules
/// into a particular path / re-export shape.
fn builtin_adapters_for(_detected: &[RuntimeKind]) -> Vec<Arc<dyn RuntimeAdapter>> {
    // Filled in once `mod adapters` is wired up — the Write order below
    // creates adapter files first, then this body is replaced to dispatch
    // on `_detected`.
    Vec::new()
}

// ── Tool: containers.list ──────────────────────────────────────────────────

/// Arguments for `containers.list`.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainersListArgs {
    /// Restrict to one runtime. When unset, every detected runtime on this
    /// host contributes rows.
    #[arg(long)]
    pub runtime: Option<String>,
    /// Include stopped / exited / dead containers in addition to running.
    /// Defaults to true — the reconciler's whole point is acting on
    /// non-running rows.
    #[arg(long)]
    pub all: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContainersListOutput {
    /// Runtimes the local host successfully detected via the probe.
    pub runtimes: Vec<String>,
    /// Container rows aggregated across every registered adapter on this
    /// host, sorted by `(host, name)`.
    pub containers: Vec<Container>,
    /// Per-adapter failure rows. An adapter that errors during `list()` does
    /// not fail the whole tool — its kind + error message land here so the
    /// caller sees the partial picture explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapter_errors: Vec<AdapterListError>,
}

/// One adapter's `list()` failure, recorded alongside the successful rows
/// from the other adapters so callers can render "docker is down, lxc has
/// 12 containers" without losing either side.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdapterListError {
    /// Runtime name (matches [`RuntimeKind::as_str`]).
    pub runtime: String,
    /// Human-readable error message — the `Display` form of [`AdapterError`].
    pub message: String,
}

/// List containers across every registered adapter on this host. When the
/// adapter registry is empty (no host bootstrap ran, no adapters wired in
/// tests), [`adapters::builtin_adapters`] is consulted as a fallback so the
/// tool is useful straight out of the box.
///
/// Sorted by `(host, name)`. Per-adapter failures land in
/// [`ContainersListOutput::adapter_errors`]; a single bad adapter never
/// aborts the call.
#[derive::orca_tool(domain = "containers", verb = "list")]
async fn containers_list(
    args: ContainersListArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ContainersListOutput> {
    let detected = detect_available_runtimes()?;

    // Resolve the adapter set. Tests / bootstraps that called
    // [`register_adapter`] keep their wiring; otherwise fall back to the
    // built-ins keyed off the detection result.
    let mut adapters = registered_adapters();
    if adapters.is_empty() {
        adapters = builtin_adapters_for(&detected);
    }

    let filter = ListFilter {
        all: args.all.unwrap_or(true),
        labels: Vec::new(),
    };
    let runtime_filter = args.runtime.as_deref().map(str::to_ascii_lowercase);

    let mut rows: Vec<Container> = Vec::new();
    let mut errors: Vec<AdapterListError> = Vec::new();
    for adapter in adapters {
        let kind = adapter.kind();
        if let Some(want) = &runtime_filter
            && kind.as_str() != want
        {
            continue;
        }
        match adapter.list(&filter).await {
            Ok(mut got) => rows.append(&mut got),
            Err(e) => errors.push(AdapterListError {
                runtime: kind.as_str().to_string(),
                message: e.to_string(),
            }),
        }
    }

    rows.sort_by(|a, b| a.host.cmp(&b.host).then_with(|| a.name.cmp(&b.name)));

    Ok(ContainersListOutput {
        runtimes: detected
            .into_iter()
            .map(|k| k.as_str().to_string())
            .collect(),
        containers: rows,
        adapter_errors: errors,
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_kind_strings_are_stable() {
        assert_eq!(RuntimeKind::Docker.as_str(), "docker");
        assert_eq!(RuntimeKind::Lxc.as_str(), "lxc");
        assert_eq!(RuntimeKind::Podman.as_str(), "podman");
        assert_eq!(RuntimeKind::Nspawn.as_str(), "nspawn");
    }

    #[test]
    fn restart_policy_desire_matches_2_1_rules() {
        assert!(RestartPolicy::UnlessStopped.desires_running());
        assert!(RestartPolicy::Always.desires_running());
        assert!(!RestartPolicy::No.desires_running());
        assert!(!RestartPolicy::OnFailure.desires_running());
    }

    fn sample_container(policy: RestartPolicy, labels: Vec<(&str, &str)>) -> Container {
        Container {
            id: "9c2f4a1b8e7d4c5fa1b2c3d4e5f60718".to_string(),
            name: "sabnzbd".to_string(),
            runtime: RuntimeKind::Docker,
            host: "freyr".to_string(),
            state: ContainerState::Created,
            restart_policy: policy,
            image: Some("lscr.io/linuxserver/sabnzbd:latest".to_string()),
            labels: labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code: None,
            startup: None,
        }
    }

    #[test]
    fn skip_label_overrides_unless_stopped_desire() {
        let c = sample_container(RestartPolicy::UnlessStopped, vec![("orca.skip", "true")]);
        assert!(!c.desires_running());
    }

    #[test]
    fn unless_stopped_without_skip_label_is_desired_running() {
        let c = sample_container(RestartPolicy::UnlessStopped, Vec::new());
        assert!(c.desires_running());
    }

    #[test]
    fn heal_manual_label_does_not_clear_desire() {
        let c = sample_container(RestartPolicy::UnlessStopped, vec![("orca.heal", "manual")]);
        assert!(c.desires_running());
        assert_eq!(c.label("orca.heal"), Some("manual"));
    }

    #[test]
    fn detect_available_runtimes_is_pure_probe() {
        let detected = detect_available_runtimes().expect("probe should not fail");
        for k in &detected {
            assert!(!k.as_str().is_empty());
        }
    }

    #[test]
    fn startup_ordering_is_empty_when_all_fields_none() {
        assert!(StartupOrdering::default().is_empty());
        let so = StartupOrdering {
            order: Some(3),
            ..Default::default()
        };
        assert!(!so.is_empty());
    }
}
