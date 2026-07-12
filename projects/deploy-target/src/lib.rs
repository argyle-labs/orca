//! Generic deploy-target domain. One model, one adapter trait, one registry —
//! many targets (Proxmox VM/LXC, Docker, Dockge, Podman, a raw compose file).
//!
//! orca does not care *what kind* of runtime a target is; it cares that it can
//! place a workload somewhere and observe it. A plugin contributes a target
//! ("I can run workloads as LXC on this Proxmox node") plus the capabilities it
//! supports (launch/stop/logs/shell/migrate/…). Consumers (the `deploy.*`
//! tools, the future cross-runtime migration engine) iterate the registered
//! targets rather than reaching for `proxmox`/`docker`/`dockge` by name.
//!
//! Follows the same plug-in shape as `storage` and `notifications`: a
//! [`DeployTarget`] trait + a process-global registry every adapter registers
//! itself against at bootstrap, plus a JSON-proxy [`register_from_def`] path so
//! a subprocess plugin can contribute a target over the wire `invoke` boundary.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};
use thiserror::Error;

// ── Model ───────────────────────────────────────────────────────────────────
//
// A deploy target's identity is the COMPOSITE of three INDEPENDENT axes:
// `host` × `runtime` × `kind`. They are never flattened into a single
// hyphenated identifier — doing so would hardcode the combination and stop the
// same workload from being addressed as "this runtime, that host, the other
// management surface" independently. The registry keys on the discrete triple.
//
//   host    = the machine the workload runs on (a free-form name: `host-a`,
//             `host-b`, `host-e`, …; orca does not enumerate hosts).
//   runtime = what actually executes the workload (Docker/Podman/Lxc/Vm).
//   kind    = how orca provisions/manages it on that runtime (Dockge, a raw
//             compose file, Proxmox pct/qm, a podman quadlet, a plain CLI).
//
// So `host-a`+`docker`+`dockge` and `host-a`+`docker`+`cli` are two distinct
// targets that share a host and a runtime but differ in management kind.

/// What actually executes a workload. Orthogonal to [`TargetKind`] (the
/// management surface) and to the host. This is the axis the migration engine's
/// per-runtime translators key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// Docker engine.
    Docker,
    /// Podman engine (rootful or rootless).
    Podman,
    /// System container (Proxmox `pct` LXC, …).
    Lxc,
    /// Full virtual machine (Proxmox `qm`, libvirt, …).
    Vm,
}

/// How orca provisions and manages a workload on a given [`Runtime`]. Orthogonal
/// to the runtime: e.g. a `Docker` runtime can be driven via `Dockge`, a raw
/// `Compose` file, or the plain `Cli`. An `Lxc`/`Vm` runtime is driven via
/// `Proxmox`. Kept open-ended so new management surfaces don't force a runtime
/// change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// Driven directly via the runtime's own CLI/API (plain `docker`/`podman`).
    Cli,
    /// Managed through a Dockge instance (compose stacks behind Dockge's API).
    Dockge,
    /// A `docker-compose.yml` / compose project applied directly.
    Compose,
    /// Provisioned through Proxmox (`pct` for LXC, `qm` for VMs).
    Proxmox,
    /// A systemd/podman quadlet unit.
    Quadlet,
}

/// A capability a target supports. Consumers check these before invoking an
/// operation so an unsupported call fails fast rather than at the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeployCapability {
    /// Provision + start a workload on this target.
    Launch,
    /// Stop a running workload (without destroying it).
    Stop,
    /// Restart a workload.
    Restart,
    /// Stream/tail the workload's logs.
    Logs,
    /// Open an interactive/exec shell into the workload.
    Shell,
    /// Report resource metrics for the workload.
    Metrics,
    /// Snapshot the workload (pre-migration restore point).
    Snapshot,
    /// Accept a workload migrated in from another runtime (the receiving half
    /// of the cross-runtime migration engine).
    Migrate,
}

/// The composite identity of a deploy target: the discrete `(host, runtime,
/// kind)` triple. This is the registry key — compared field-by-field, never
/// collapsed into a single string. Cheap to clone and `Hash`/`Eq` so it can key
/// maps and dedupe registrations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TargetId {
    /// The machine the workload runs on. Free-form (orca does not enumerate
    /// hosts): `host-a`, `host-b`, `host-e`, `host-f`, …
    pub host: String,
    pub runtime: Runtime,
    pub kind: TargetKind,
}

/// A deploy target as registered with orca: its composite [`TargetId`], the
/// endpoint it reaches, and the capabilities it advertises. This is the row
/// `deploy.list` surfaces and the topology aggregator turns into a node.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Target {
    /// Composite identity — `(host, runtime, kind)`, the three independent axes.
    pub id: TargetId,
    /// Human-readable endpoint, e.g. `proxmox:pve/lxc`, `docker:unix:///…`,
    /// `dockge://host:5001`. Never contains secrets.
    pub endpoint: String,
    pub capabilities: Vec<DeployCapability>,
}

/// A workload as orca asks a target to place it. Runtime-agnostic on purpose:
/// the same descriptor is handed to any target kind, and each adapter binds it
/// to its native form (compose service, LXC config, qm/cloud-init, quadlet).
/// This is the seed of the Stage-4 portable workload spec — kept intentionally
/// small for Stage 1 (launch round-trips it; richer fields land with migrate).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct WorkloadSpec {
    /// Stable workload name within the target.
    pub name: String,
    /// Image / template / rootfs source (`docker.io/lib/x`, an LXC template,
    /// a VM image id). Interpreted by the receiving adapter.
    #[serde(default)]
    pub image: Option<String>,
    /// Environment variables to inject (never secrets in plaintext — secret
    /// refs are resolved by the adapter at apply time).
    #[serde(default)]
    pub env: Vec<EnvVar>,
    /// Mount/volume bindings (`source:target`).
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Published ports (`host:container`).
    #[serde(default)]
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Mount {
    /// Host source path (or named volume / NAS export).
    pub source: String,
    /// In-workload target path.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// Outcome of a [`DeployTarget::launch`] / `stop` / `restart` operation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeployOutcome {
    /// The workload name acted on.
    pub workload: String,
    /// Runtime-native id the target assigned (CT id, container id, VMID).
    #[serde(default)]
    pub id: Option<String>,
    /// State after the operation (`running`, `stopped`, …), best-effort.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Error)]
pub enum DeployError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("capability not supported by target `{0}`: {1:?}")]
    Unsupported(String, DeployCapability),
    #[error("workload not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

// ── Target trait ──────────────────────────────────────────────────────────

/// A deploy-target adapter. proxmox implements Lxc/Vm targets; docker/dockge
/// implement container targets. A single adapter is one concrete `(host,
/// runtime, kind)` target. Default trait methods return
/// [`DeployError::Unsupported`] so a target only overrides the operations its
/// [`DeployTarget::capabilities`] advertise.
#[async_trait]
pub trait DeployTarget: Send + Sync {
    /// The machine this target runs workloads on.
    fn host(&self) -> &str;
    /// What executes the workload (Docker/Podman/Lxc/Vm).
    fn runtime(&self) -> Runtime;
    /// How orca provisions/manages it on that runtime (Dockge/Compose/…).
    fn kind(&self) -> TargetKind;
    fn capabilities(&self) -> Vec<DeployCapability>;

    /// Composite identity — the `(host, runtime, kind)` triple. This is the
    /// registry key; it is compared field-by-field, never flattened to a string.
    fn id(&self) -> TargetId {
        TargetId {
            host: self.host().to_string(),
            runtime: self.runtime(),
            kind: self.kind(),
        }
    }

    /// Target descriptor for `deploy.list` / topology.
    fn target(&self) -> Target {
        Target {
            id: self.id(),
            endpoint: self.endpoint(),
            capabilities: self.capabilities(),
        }
    }

    /// Non-secret endpoint string for display.
    fn endpoint(&self) -> String;

    fn supports(&self, cap: DeployCapability) -> bool {
        self.capabilities().contains(&cap)
    }

    async fn launch(&self, _spec: &WorkloadSpec) -> Result<DeployOutcome, DeployError> {
        Err(DeployError::Unsupported(
            self.id().describe(),
            DeployCapability::Launch,
        ))
    }

    async fn stop(&self, _workload: &str) -> Result<DeployOutcome, DeployError> {
        Err(DeployError::Unsupported(
            self.id().describe(),
            DeployCapability::Stop,
        ))
    }

    async fn restart(&self, _workload: &str) -> Result<DeployOutcome, DeployError> {
        Err(DeployError::Unsupported(
            self.id().describe(),
            DeployCapability::Restart,
        ))
    }
}

impl TargetId {
    /// A human-readable rendering of the triple for logs/errors ONLY. This is a
    /// display string, never an identifier — code matches on the discrete
    /// `host`/`runtime`/`kind` fields, never by parsing this back.
    pub fn describe(&self) -> String {
        format!(
            "host={} runtime={:?} kind={:?}",
            self.host, self.runtime, self.kind
        )
    }
}

// ── Process-global registry ─────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn DeployTarget>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a deploy target with the process-global registry. Each adapter
/// (proxmox, docker, dockge, …) calls this from its bootstrap once per
/// configured target. Re-registering the same `(host, runtime, kind)` triple
/// replaces the existing entry so a dev rebuild / reconnect doesn't duplicate
/// targets.
pub fn register_target(target: Arc<dyn DeployTarget>) {
    let mut g = GLOBAL.write().expect("deploy-target registry poisoned");
    let id = target.id();
    if let Some(slot) = g.iter_mut().find(|t| t.id() == id) {
        *slot = target;
    } else {
        g.push(target);
    }
}

/// Snapshot of every registered target. Consumers iterate this rather than
/// naming specific runtime kinds.
pub fn targets() -> Vec<Arc<dyn DeployTarget>> {
    GLOBAL
        .read()
        .expect("deploy-target registry poisoned")
        .clone()
}

/// Look up a single target by its composite identity.
pub fn target(id: &TargetId) -> Option<Arc<dyn DeployTarget>> {
    GLOBAL
        .read()
        .expect("deploy-target registry poisoned")
        .iter()
        .find(|t| &t.id() == id)
        .cloned()
}

/// Descriptor rows for every registered target — the `deploy.list` view.
pub fn descriptors() -> Vec<Target> {
    targets().iter().map(|t| t.target()).collect()
}

/// Deregister the target identified by `id`, if present. The removal path the
/// reload/unload flow needs: a plugin's domain-registration must be reversible
/// so unloading a plugin drops its targets from the registry rather than
/// leaving stale rows pointing at an invoke thunk whose plugin is gone.
/// Returns `true` if a target was removed.
pub fn deregister_target(id: &TargetId) -> bool {
    let mut g = GLOBAL.write().expect("deploy-target registry poisoned");
    let before = g.len();
    g.retain(|t| &t.id() != id);
    before != g.len()
}

/// Deregister *every* target on `host`, returning how many were removed. The
/// loader's plugin-unload path is keyed by the backend `name` it recorded at
/// load, which for this domain is the host axis; a single plugin may have
/// registered several `(runtime, kind)` targets under that host, so unloading
/// it must drop all of them. Used by the loader; the precise
/// [`deregister_target`] is for targeted removal (e.g. after a migration
/// retires a source).
pub fn deregister_host(host: &str) -> usize {
    let mut g = GLOBAL.write().expect("deploy-target registry poisoned");
    let before = g.len();
    g.retain(|t| t.host() != host);
    before - g.len()
}

/// The synchronous invoke thunk a loaded plugin's domain target is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. The loader
/// supplies a closure that marshals `op` into a `"{invoke_prefix}.{op}"` tool
/// call over the subprocess wire. Kept as a plain `Fn` of strings so
/// this crate stays free of any dependency on the loader crates (no cycle):
/// the loader owns the transport, deploy-target owns the domain shape.
///
/// Host-side (in-process) only: the thunk drives a *loaded plugin* over the
/// subprocess wire — a daemon/host concern. A thin subprocess plugin links no loader
/// path and no tokio, so the whole proxy surface is gated out on thin,
/// consistent with `http`/`db` being capabilities rather than always-linked.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> Result<String, DeployError> + Send + Sync + 'static>;

/// Build and register a [`DeployTarget`] from a plugin's backend descriptor plus
/// an [`InvokeThunk`]. The loader calls this from its domain dispatch table; it
/// parses the discrete `host` / `runtime` / `kind` / `capabilities` strings into
/// the domain enums and wires every advertised operation back through `invoke`.
///
/// The three identity axes arrive as separate strings — never one composite
/// token — so the registry keys on the discrete triple. Unknown enum values are
/// rejected so a typo surfaces at load, not at first use. Registration replaces
/// any existing target with the same triple (idempotent reload), matching
/// [`register_target`]'s semantics.
#[cfg(feature = "in-process")]
pub fn register_from_def(
    host: String,
    runtime: &str,
    kind: &str,
    endpoint: String,
    capabilities: &[String],
    invoke: InvokeThunk,
) -> Result<(), DeployError> {
    let runtime = parse_runtime(runtime)?;
    let kind = parse_kind(kind)?;
    let capabilities = capabilities
        .iter()
        .map(|c| parse_capability(c))
        .collect::<Result<Vec<_>, _>>()?;
    register_target(Arc::new(DeployProxy {
        host,
        runtime,
        kind,
        endpoint,
        capabilities,
        invoke,
    }));
    Ok(())
}

// Parse helpers exist only to validate a loaded plugin's `BackendDef` strings in
// `register_from_def`, so they gate with the proxy. Thin plugins declare their
// axes in Rust, not as loader strings.
#[cfg(feature = "in-process")]
fn parse_runtime(s: &str) -> Result<Runtime, DeployError> {
    match s {
        "docker" => Ok(Runtime::Docker),
        "podman" => Ok(Runtime::Podman),
        "lxc" => Ok(Runtime::Lxc),
        "vm" => Ok(Runtime::Vm),
        other => Err(DeployError::Other(format!(
            "unknown deploy-target runtime `{other}`"
        ))),
    }
}

#[cfg(feature = "in-process")]
fn parse_kind(s: &str) -> Result<TargetKind, DeployError> {
    match s {
        "cli" => Ok(TargetKind::Cli),
        "dockge" => Ok(TargetKind::Dockge),
        "compose" => Ok(TargetKind::Compose),
        "proxmox" => Ok(TargetKind::Proxmox),
        "quadlet" => Ok(TargetKind::Quadlet),
        other => Err(DeployError::Other(format!(
            "unknown deploy-target kind `{other}`"
        ))),
    }
}

#[cfg(feature = "in-process")]
fn parse_capability(s: &str) -> Result<DeployCapability, DeployError> {
    match s {
        "launch" => Ok(DeployCapability::Launch),
        "stop" => Ok(DeployCapability::Stop),
        "restart" => Ok(DeployCapability::Restart),
        "logs" => Ok(DeployCapability::Logs),
        "shell" => Ok(DeployCapability::Shell),
        "metrics" => Ok(DeployCapability::Metrics),
        "snapshot" => Ok(DeployCapability::Snapshot),
        "migrate" => Ok(DeployCapability::Migrate),
        other => Err(DeployError::Other(format!(
            "unknown deploy-target capability `{other}`"
        ))),
    }
}

/// A [`DeployTarget`] backed by a subprocess plugin reached over the JSON-proxy
/// wire. Each async trait method serializes its args to JSON, offloads the
/// synchronous [`InvokeThunk`] onto `spawn_blocking` (so a slow/wedged plugin
/// never blocks the async runtime), and deserializes the JSON result.
#[cfg(feature = "in-process")]
struct DeployProxy {
    host: String,
    runtime: Runtime,
    kind: TargetKind,
    endpoint: String,
    capabilities: Vec<DeployCapability>,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl DeployProxy {
    /// Run one proxied op on the blocking pool and deserialize its JSON result.
    /// `op` is the bare operation name (the loader's thunk prepends the plugin's
    /// invoke prefix); `args` is the op's typed args object.
    async fn call<A, R>(&self, op: &'static str, args: A) -> Result<R, DeployError>
    where
        A: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| DeployError::Other(format!("encode `{op}` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
            .await
            .map_err(|e| DeployError::Transport(format!("`{op}` proxy task failed: {e}")))??;
        serde_json::from_str(&out)
            .map_err(|e| DeployError::Other(format!("decode `{op}` result: {e}")))
    }
}

#[cfg(feature = "in-process")]
#[async_trait]
impl DeployTarget for DeployProxy {
    fn host(&self) -> &str {
        &self.host
    }
    fn runtime(&self) -> Runtime {
        self.runtime
    }
    fn kind(&self) -> TargetKind {
        self.kind
    }
    fn capabilities(&self) -> Vec<DeployCapability> {
        self.capabilities.clone()
    }
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    async fn launch(&self, spec: &WorkloadSpec) -> Result<DeployOutcome, DeployError> {
        self.call("launch", LaunchArgs { spec: spec.clone() }).await
    }

    async fn stop(&self, workload: &str) -> Result<DeployOutcome, DeployError> {
        self.call(
            "stop",
            WorkloadArg {
                workload: workload.to_string(),
            },
        )
        .await
    }

    async fn restart(&self, workload: &str) -> Result<DeployOutcome, DeployError> {
        self.call(
            "restart",
            WorkloadArg {
                workload: workload.to_string(),
            },
        )
        .await
    }
}

// ── Proxy wire-args ───────────────────────────────────────────────────────
// Typed args objects each proxied op serializes across the FFI invoke boundary.
// Defined (not `json!`'d) so the wire contract is explicit and a plugin's
// `invoke` arm deserializes against the same shape — no opaque `Value`.

// Encoded only by the host-side `DeployProxy` (deploy-target has no plugin-side
// `dispatch_op`), so both gate with the proxy — thin links neither.
#[cfg(feature = "in-process")]
#[derive(Serialize, Deserialize)]
struct LaunchArgs {
    spec: WorkloadSpec,
}

#[cfg(feature = "in-process")]
#[derive(Serialize, Deserialize)]
struct WorkloadArg {
    workload: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pve_lxc_id() -> TargetId {
        TargetId {
            host: "host-b".into(),
            runtime: Runtime::Lxc,
            kind: TargetKind::Proxmox,
        }
    }

    struct FakeProxmox {
        host: String,
    }

    #[async_trait]
    impl DeployTarget for FakeProxmox {
        fn host(&self) -> &str {
            &self.host
        }
        fn runtime(&self) -> Runtime {
            Runtime::Lxc
        }
        fn kind(&self) -> TargetKind {
            TargetKind::Proxmox
        }
        fn capabilities(&self) -> Vec<DeployCapability> {
            vec![DeployCapability::Launch, DeployCapability::Stop]
        }
        fn endpoint(&self) -> String {
            "proxmox:pve/lxc".into()
        }
        async fn launch(&self, spec: &WorkloadSpec) -> Result<DeployOutcome, DeployError> {
            Ok(DeployOutcome {
                workload: spec.name.clone(),
                id: Some("101".into()),
                state: Some("running".into()),
                detail: None,
            })
        }
    }

    #[test]
    fn register_and_snapshot_round_trip() {
        let id = pve_lxc_id();
        deregister_target(&id);
        register_target(Arc::new(FakeProxmox {
            host: "host-b".into(),
        }));
        let found = target(&id).expect("registered target is retrievable");
        assert_eq!(found.runtime(), Runtime::Lxc);
        assert_eq!(found.kind(), TargetKind::Proxmox);
        assert!(found.supports(DeployCapability::Launch));
        assert!(!found.supports(DeployCapability::Migrate));

        let row = descriptors()
            .into_iter()
            .find(|t| t.id == id)
            .expect("descriptor surfaces in deploy.list view");
        assert_eq!(row.id.runtime, Runtime::Lxc);
        assert_eq!(row.endpoint, "proxmox:pve/lxc");

        // Re-register same triple replaces rather than duplicates.
        register_target(Arc::new(FakeProxmox {
            host: "host-b".into(),
        }));
        assert_eq!(descriptors().iter().filter(|t| t.id == id).count(), 1);

        assert!(deregister_target(&id));
        assert!(target(&id).is_none());
    }

    #[test]
    fn same_host_runtime_different_kind_are_distinct() {
        // host-a + docker + dockge  vs  host-a + docker + cli: same host and
        // runtime, different management kind → two separate targets. This is
        // exactly why identity can't be a single flattened name.
        let dockge = TargetId {
            host: "host-a".into(),
            runtime: Runtime::Docker,
            kind: TargetKind::Dockge,
        };
        let cli = TargetId {
            host: "host-a".into(),
            runtime: Runtime::Docker,
            kind: TargetKind::Cli,
        };
        assert_ne!(dockge, cli);
    }

    #[test]
    fn unsupported_op_fails_fast() {
        let t = FakeProxmox { host: "x".into() };
        // restart not in capabilities → default trait impl refuses.
        let err = futures_block(t.restart("foo"));
        assert!(matches!(
            err,
            Err(DeployError::Unsupported(_, DeployCapability::Restart))
        ));
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn parse_rejects_unknown_axes() {
        assert!(parse_runtime("toaster").is_err());
        assert!(parse_kind("teleport").is_err());
        assert!(parse_capability("teleport").is_err());
        assert!(parse_runtime("lxc").is_ok());
        assert!(parse_kind("dockge").is_ok());
        assert!(parse_capability("migrate").is_ok());
    }

    // Minimal blocking executor for the trait's async default-method test
    // without pulling a runtime into a unit test.
    fn futures_block<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VT)
        }
        static VT: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VT)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }
}
