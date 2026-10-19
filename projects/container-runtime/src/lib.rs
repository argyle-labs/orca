#![allow(clippy::disallowed_types)]

use derive::orca_async;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::LazyLock;
use thiserror::Error;

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

    /// Parse the stable short string back into a [`RuntimeKind`]. Inverse of
    /// [`RuntimeKind::as_str`]; used by the FFI seam to reconstruct a plugin
    /// backend's runtime kind from its `BackendDef::kind`.
    // Inherent constructor mirroring `as_str`; returns `Option` (not `Result`),
    // so the `FromStr` trait is a poor fit — keep the symmetric inherent name.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "docker" => Some(RuntimeKind::Docker),
            "lxc" => Some(RuntimeKind::Lxc),
            "podman" => Some(RuntimeKind::Podman),
            "nspawn" => Some(RuntimeKind::Nspawn),
            _ => None,
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
#[derive::snake_aliases]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerMount {
    /// Host-side path (docker `Source`, lxc `mp*` host part).
    pub source: PathBuf,
    /// Container-side path.
    pub target: PathBuf,
    /// True when the mount is read-only.
    pub read_only: bool,
}

/// One open published port (host:container, protocol).
#[derive::snake_aliases]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[derive::snake_aliases]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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
#[derive::snake_aliases]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
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
    /// Operational health, populated by runtime adapters per the layered
    /// precedence rule in [`contract::health`]. Defaults to
    /// [`contract::health::Health::Unknown`] until an adapter reports.
    #[serde(default)]
    pub health: contract::health::Health,
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
#[derive(Debug, Error, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct LogTail(pub u32);

impl Default for LogTail {
    fn default() -> Self {
        Self(200)
    }
}

/// Result of a one-shot [`RuntimeAdapter::exec`]: the captured streams plus the
/// command's exit status. `stdout`/`stderr` are best-effort UTF-8 (lossy);
/// `exit_code` is `None` only when the runtime couldn't report one (e.g. the
/// process was still attached when the stream closed).
#[derive::snake_aliases]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerExecOutput {
    /// Process exit code, when the runtime reported it.
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
}

/// Previous name of [`ContainerExecOutput`], kept until every out-of-tree plugin
/// has been rebuilt against the new name.
pub type ExecOutput = ContainerExecOutput;

/// The surface every runtime adapter implements. Methods are intentionally
/// the minimum set §2.1 and §2.2 need.
///
/// `kind()` exists so the reconciler can stamp [`Container::runtime`]
/// correctly when it stitches adapter results into the unified view, and so
/// log lines / notifications can name the responsible runtime without a
/// downcast.
#[orca_async]
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

    /// Run `cmd` inside the container/CT once and return its captured output.
    /// `stdin`, when `Some`, is fed to the process's standard input. This is a
    /// one-shot exec (no TTY, no interactive session) — the building block for
    /// `containers.exec` and, later, the migration engine's in-guest steps.
    ///
    /// Default returns [`AdapterError::Refused`] so a runtime without an exec
    /// path (or one not yet wired) fails loudly rather than silently no-oping.
    async fn exec(
        &self,
        _id: &str,
        _cmd: &[String],
        _stdin: Option<String>,
    ) -> Result<ContainerExecOutput, AdapterError> {
        Err(AdapterError::Refused(
            "exec not supported by this runtime adapter".into(),
        ))
    }

    /// Gather a per-container [`HostObservation`] for the breaker. The
    /// default returns an empty observation — only adapters whose
    /// runtime carries breaker-relevant out-of-band signals
    /// (currently lxc's `journalctl` tail) override it. `lxc_previous_state`
    /// is *not* the adapter's concern: the breaker owns cross-tick state
    /// in `containers::breaker::BreakerRecord::last_observed_state` and
    /// injects it inside `containers::breaker::arm`. Errors gathering the
    /// observation are intentionally swallowed in the override (logged
    /// via `tracing`) — a missing journal tail must not block a start.
    async fn observe(&self, _container: &Container) -> HostObservation {
        HostObservation::default()
    }

    /// Probe whether the container's userspace is responsive. Default
    /// returns [`Liveness::NotApplicable`] — only adapters with a way
    /// to reach inside the container override it. Implementations must
    /// respect a tight budget (≤5s) because the reconciler can call
    /// this every tick on every running container.
    ///
    /// The distinction this enables: `ContainerState::Running` is
    /// "runtime thinks it's up"; `Liveness::Live` is "userspace
    /// answered". A `Running` + `Wedged` container (PID 1 hung, ffprobe
    /// chewing CPU, no service ports responding) is the failure mode
    /// that motivated this surface — see [[feedback-api-first-liveness-exception]].
    async fn probe_liveness(&self, _container: &Container) -> Liveness {
        Liveness::NotApplicable
    }

    /// Hook returning a [`WedgeRecoverer`] for adapters that can attempt
    /// in-place recovery from a wedged container. Default: not
    /// supported (the reconciler will escalate straight to
    /// `containers.wedged_unrecoverable` after K detections).
    fn wedge_recoverer(&self) -> Option<&dyn WedgeRecoverer> {
        None
    }
}

/// Liveness observation produced by [`RuntimeAdapter::probe_liveness`].
///
/// Sibling to [`ContainerState`] — kept separate so "runtime says
/// running" and "userspace responded" remain distinguishable. The
/// reconciler keys auto-recovery off the `Wedged` value (see the
/// `wedge` module).
///
/// Lives in a side observation (not on `Container`) so existing
/// adapters don't have to learn a new field, and so the value isn't
/// stamped into wire payloads that don't need it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Liveness {
    /// Adapter has no liveness concept for this runtime, or the
    /// container isn't `Running` and the question doesn't apply.
    #[default]
    NotApplicable,
    /// Probe succeeded within budget — userspace answered.
    Live,
    /// Probe ran but exceeded its timeout — PID 1 wedged.
    Wedged,
    /// Probe failed for a non-timeout reason (spawn error, non-zero
    /// exit, container vanished during the probe). Treated as "do not
    /// act" by the reconciler — we don't escalate on a transient
    /// adapter glitch.
    Unknown,
}

impl Liveness {
    /// Stable short string used in tool output, log lines, and route
    /// matchers. Mirrors [`RuntimeKind::as_str`] / [`ContainerState`]
    /// shape.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Live => "live",
            Self::Wedged => "wedged",
            Self::Unknown => "unknown",
        }
    }
}

/// In-place recovery from a wedged container. Implementations live on
/// the same adapter (returned via [`RuntimeAdapter::wedge_recoverer`])
/// so a single instance answers both "is it wedged?" and "fix it."
///
/// `attempt_unwedge` returns `Ok` only when the implementation
/// believes recovery has been attempted to completion — the caller
/// re-probes liveness to decide whether the container actually came
/// back. The split keeps "I tried" separate from "it worked" so the
/// state machine can distinguish failed RPCs from successful RPCs
/// that didn't unstick the container.
#[orca_async]
pub trait WedgeRecoverer: Send + Sync {
    /// Attempt to bring `container` back to a live state. Idempotent
    /// against an already-live container (return `Ok(())`).
    async fn attempt_unwedge(&self, container: &Container) -> Result<(), AdapterError>;
}

// ── Hostname capture ───────────────────────────────────────────────────────

/// Lightweight `hostname` lookup for stamping [`Container::host`]. Matches
/// the behavior of `system::host_identity::capture_hostname` — shells the
/// `hostname` binary, falls back to `"unknown"` if unavailable — without
/// taking a dep on the `system` crate (which depends on us indirectly).
///
/// Computed once per process via [`LazyLock`].
/// Public so container-runtime plugins (docker/…) can stamp `Container.host`
/// with the same host identity the core reconciler keys on.
pub fn local_hostname() -> &'static str {
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

/// Per-tick observation captured by the caller. Runtime-specific signals
/// the classifier can't synthesize from a single `Container` row land
/// here. Optional fields stay `None` when the runtime doesn't have a
/// corresponding signal cheap enough to gather every tick.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HostObservation {
    /// Raw output of `journalctl -u pve-container@<vmid>.service
    /// --since "5 min ago" --no-pager -o cat`. Populated by the LXC
    /// arming path only.
    ///
    /// Filter (case-insensitive, matched per line):
    /// - contains `failed to start`
    /// - OR contains `exited with status`
    pub lxc_journal_tail: Option<String>,
    /// Previous observed state of the LXC, if known. The reconciler
    /// passes this in so the classifier can detect a stopped→running
    /// transition without owning a separate persistent state field.
    pub lxc_previous_state: Option<ContainerState>,
}

/// Capability string a `container_runtime` backend sets in its `BackendDef` to
/// declare it can attempt in-place wedge recovery.
pub const CAP_WEDGE_RECOVER: &str = "wedge_recover";

// ── Wire arg shapes (the designated FFI dispatch seam) ───────────────────────

#[derive(Serialize, Deserialize)]
pub struct IdArg {
    pub id: String,
}

#[derive(Serialize, Deserialize)]
pub struct LogsArg {
    pub id: String,
    pub tail: LogTail,
}

#[derive(Serialize, Deserialize)]
pub struct ExecArg {
    pub id: String,
    pub cmd: Vec<String>,
    pub stdin: Option<String>,
}

// ── Plugin-side dispatch ──────────────────────────────────────────────────────

/// Route a proxied `op` to a plugin's own `dyn RuntimeAdapter` and encode the
/// result for the FFI boundary. Symmetric with [`ContainerRuntimeProxy`] — a
/// plugin's `backend_dispatch` delegates here so it never hand-writes the op
/// match. Errors are JSON-encoded [`AdapterError`]s so the host proxy can
/// reconstruct the variant.
pub async fn dispatch_op(
    adapter: &dyn RuntimeAdapter,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    fn dec<T: DeserializeOwned>(op: &str, args: serde_json::Value) -> Result<T, serde_json::Value> {
        serde_json::from_value(args)
            .map_err(|e| enc_err(&AdapterError::Malformed(format!("decode {op} args: {e}"))))
    }
    fn enc<T: Serialize>(v: &T) -> Result<serde_json::Value, serde_json::Value> {
        serde_json::to_value(v)
            .map_err(|e| serde_json::Value::String(format!("encode result: {e}")))
    }
    fn enc_err(e: &AdapterError) -> serde_json::Value {
        serde_json::to_value(e).unwrap_or_else(|_| serde_json::Value::String(e.to_string()))
    }

    match op {
        "list" => {
            let filter: ListFilter = dec(op, args)?;
            adapter
                .list(&filter)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "inspect" => {
            let a: IdArg = dec(op, args)?;
            adapter
                .inspect(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "start" => {
            let a: IdArg = dec(op, args)?;
            adapter
                .start(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "stop" => {
            let a: IdArg = dec(op, args)?;
            adapter
                .stop(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "restart" => {
            let a: IdArg = dec(op, args)?;
            adapter
                .restart(&a.id)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|()| enc(&()))
        }
        "logs" => {
            let a: LogsArg = dec(op, args)?;
            adapter
                .logs(&a.id, a.tail)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "exec" => {
            let a: ExecArg = dec(op, args)?;
            adapter
                .exec(&a.id, &a.cmd, a.stdin)
                .await
                .map_err(|e| enc_err(&e))
                .and_then(|v| enc(&v))
        }
        "observe" => {
            let c: Container = dec(op, args)?;
            enc(&adapter.observe(&c).await)
        }
        "probe_liveness" => {
            let c: Container = dec(op, args)?;
            enc(&adapter.probe_liveness(&c).await)
        }
        "attempt_unwedge" => {
            let c: Container = dec(op, args)?;
            match adapter.wedge_recoverer() {
                Some(r) => r
                    .attempt_unwedge(&c)
                    .await
                    .map_err(|e| enc_err(&e))
                    .and_then(|()| enc(&())),
                None => Err(enc_err(&AdapterError::Refused(
                    "wedge recovery not supported by this runtime adapter".into(),
                ))),
            }
        }
        other => Err(enc_err(&AdapterError::Refused(format!(
            "unknown container_runtime op: {other}"
        )))),
    }
}

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

    #[test]
    fn startup_ordering_is_empty_when_all_fields_none() {
        assert!(StartupOrdering::default().is_empty());
        let so = StartupOrdering {
            order: Some(3),
            ..Default::default()
        };
        assert!(!so.is_empty());
    }

    #[test]
    fn runtime_kind_from_str_round_trips_every_variant() {
        for k in [
            RuntimeKind::Docker,
            RuntimeKind::Lxc,
            RuntimeKind::Podman,
            RuntimeKind::Nspawn,
        ] {
            assert_eq!(RuntimeKind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(RuntimeKind::from_str("qemu"), None);
        assert_eq!(RuntimeKind::from_str(""), None);
    }

    #[test]
    fn liveness_as_str_and_default() {
        assert_eq!(Liveness::NotApplicable.as_str(), "not_applicable");
        assert_eq!(Liveness::Live.as_str(), "live");
        assert_eq!(Liveness::Wedged.as_str(), "wedged");
        assert_eq!(Liveness::Unknown.as_str(), "unknown");
        assert_eq!(Liveness::default(), Liveness::NotApplicable);
    }

    #[test]
    fn log_tail_default_is_200() {
        assert_eq!(LogTail::default().0, 200);
    }

    #[test]
    fn exec_output_default_is_empty() {
        let out = ContainerExecOutput::default();
        assert_eq!(out.exit_code, None);
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn adapter_error_display_messages() {
        assert_eq!(
            AdapterError::NotFound("101".into()).to_string(),
            "container `101` not found"
        );
        assert_eq!(
            AdapterError::Unavailable("socket down".into()).to_string(),
            "runtime unavailable: socket down"
        );
        assert_eq!(
            AdapterError::Malformed("bad json".into()).to_string(),
            "runtime returned malformed data: bad json"
        );
        assert_eq!(
            AdapterError::Transport("io".into()).to_string(),
            "transport error: io"
        );
        assert_eq!(
            AdapterError::Refused("locked".into()).to_string(),
            "operation refused: locked"
        );
    }

    #[test]
    fn local_hostname_is_nonempty_and_trimmed() {
        let h = local_hostname();
        assert!(!h.is_empty());
        assert!(!h.ends_with('.'));
        assert!(!h.ends_with('\n'));
    }
}
