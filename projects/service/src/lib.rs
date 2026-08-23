//! Generic service domain. One model, one adapter trait, one registry — many
//! service backends (audiobookshelf, immich, opnsense, ollama, …).
//!
//! orca does not care *what* a service is; it cares that it can be deployed,
//! backed up, restored, configured, and queried. A plugin contributes a
//! [`ServiceBackend`]; the generic `service.*` tools take the service name as a
//! parameter and iterate the registered backends rather than naming any service
//! by type. This keeps the fleet's API surface at ~8 tools total instead of
//! N-per-plugin.
//!
//! **Composition, not duplication.** A service is *software*; a
//! [`deploy_target`](::deploy_target) is a *place to run software*
//! `(host, runtime, kind)`. The two never overlap: a backend describes its
//! workload as a runtime-agnostic [`WorkloadSpec`] (via [`ServiceBackend::workload_spec`])
//! and the generic `service.deploy` tool hands that spec to a registered deploy
//! target's `launch`. service therefore drives no `pct`/`docker` itself —
//! placement mechanics live once, in `deploy-target`. What service owns that
//! deploy-target cannot is the app-level lifecycle: backup, restore, configure,
//! status — all of which need service-specific knowledge.
//!
//! Mirrors the `storage`/`deploy-target` plug-in shape: a trait + a
//! process-global registry, plus a JSON-proxy FFI boundary (`ServiceProxy` /
//! [`dispatch_op`]) with a single wire contract.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};
// Used only by `stamp()` in the in-process subprocess backup path.
#[cfg(feature = "in-process")]
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
// Subprocess backup is an in-process capability; a thin plugin links no
// `tokio::process`. See the `in-process` feature.
#[cfg(feature = "in-process")]
use tokio::process::Command;

/// Object-safe async return type — the canonical hand-desugared `BoxFuture` from
/// `contract` (one definition workspace-wide; no `async_trait` macro). Re-exported
/// so existing `service::BoxFuture` paths keep working.
pub use contract::BoxFuture;

// The runtime axis + the portable workload descriptor are owned by
// deploy-target; service reuses them rather than redefining a parallel
// `Modality` enum (the duplication this domain was refactored to avoid).
pub use deploy_target::{Runtime, WorkloadSpec};

// ── Model ───────────────────────────────────────────────────────────────────

/// The lifecycle operations a backend advertises it supports. Consumers branch
/// on capability before invoking. `Deploy` means the backend can produce a
/// [`WorkloadSpec`] (i.e. it runs as a container/VM via a deploy target);
/// device/host-only services (mikrotik, a host UPS daemon) omit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServiceCapability {
    Deploy,
    Backup,
    Restore,
    Configure,
    Status,
}

impl ServiceCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceCapability::Deploy => "deploy",
            ServiceCapability::Backup => "backup",
            ServiceCapability::Restore => "restore",
            ServiceCapability::Configure => "configure",
            ServiceCapability::Status => "status",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ServiceError> {
        match s {
            "deploy" => Ok(ServiceCapability::Deploy),
            "backup" => Ok(ServiceCapability::Backup),
            "restore" => Ok(ServiceCapability::Restore),
            "configure" => Ok(ServiceCapability::Configure),
            "status" => Ok(ServiceCapability::Status),
            other => Err(ServiceError::Other(format!(
                "unknown service capability `{other}`"
            ))),
        }
    }
}

/// Parse a [`Runtime`] from its snake_case wire form, reusing deploy-target's
/// own serde mapping so the string set never forks from the enum.
pub fn parse_runtime(s: &str) -> Result<Runtime, ServiceError> {
    serde_json::from_str(&format!("\"{s}\""))
        .map_err(|_| ServiceError::Other(format!("unknown runtime `{s}`")))
}

/// Wire form of a [`Runtime`] (snake_case), for CSV round-tripping in a
/// `BackendDef`. The inverse of [`parse_runtime`].
pub fn runtime_str(r: Runtime) -> String {
    serde_json::to_string(&r)
        .ok()
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_default()
}

/// A registered, non-secret connection descriptor for one service instance. The
/// `service.connect` tool persists these (token held in the secret store); each
/// lifecycle op receives the resolved `Endpoint` for the named instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Endpoint {
    /// Instance name (`"audiobookshelf-main"`), unique within a provider. Also
    /// the runtime handle for generic backup/restore: the container name for
    /// docker/podman, or the LXC `vmid` for lxc.
    pub name: String,
    /// Base URL or host the service is reached at.
    pub base_url: String,
    /// Deploy target host (Proxmox node / docker host). Empty for already-running.
    #[serde(default)]
    pub target_host: String,
    /// Runtime this instance runs as. Drives the generic backup/restore path;
    /// when absent, the backend's first declared runtime is used.
    #[serde(default)]
    pub runtime: Option<Runtime>,
    /// Backup method to use for this instance (`"tar"`, `"pbs"`, …). Resolved
    /// against the pluggable [`BackupMethod`] registry; absent = `"tar"`.
    #[serde(default)]
    pub backup_method: Option<String>,
    /// API token / credential. Carried here for the in-process call; the
    /// `service.connect` tool stores it in the secret store, not in display.
    #[serde(default)]
    pub token: String,
}

/// A backup artifact produced by [`ServiceBackend::backup`], restorable via
/// [`ServiceBackend::restore`]. The path is on the deploy target's filesystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackupArtifact {
    pub service: String,
    pub instance: String,
    pub path: String,
    /// Sortable `YYYYMMDD-HHMMSS` stamp.
    pub timestamp: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub checksum: String,
}

/// Health/diagnostics result of [`ServiceBackend::status`].
///
/// This is how a plugin exposes ALL of its information through the single
/// `service.*` surface (and therefore to the orca MCP) — no per-plugin tools.
/// `healthy`/`detail` are the uniform summary every backend reports; `info`
/// carries arbitrary, plugin-specific structured data (a jellyfin plugin puts
/// its libraries + transcode health here, a homeassistant plugin its entities,
/// an arr plugin its indexers/health) so rich reads survive the API-surface
/// limit by riding the one generic verb instead of a bespoke tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServiceStatus {
    pub healthy: bool,
    #[serde(default)]
    pub detail: String,
    /// Plugin-specific structured detail, surfaced through `service.status`.
    /// Fully typed (never opaque JSON): a tagged enum whose variants orca owns,
    /// one per data kind, so every shape is known.
    #[serde(default)]
    pub info: ServiceInfo,
}

/// Typed, plugin-specific `service.status` detail.
///
/// HARD RULE: no opaque JSON anywhere — every plugin's rich data is modeled as a
/// concrete typed variant here, owned centrally, so the full schema is always
/// known. A variant is added as each rich plugin (jellyfin/plex media,
/// homeassistant entities, arr indexers, …) is converted to the single surface.
/// `None` is the default for backends that report only health.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServiceInfo {
    /// Backend reported only health — no structured detail.
    #[default]
    None,
}

/// Descriptor row for `service.list` / topology — a backend's own self-report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServiceProvider {
    pub name: String,
    /// Runtimes this software can be placed on (via a matching deploy target).
    pub runtimes: Vec<Runtime>,
    pub default_port: u16,
    pub endpoint: String,
    pub capabilities: Vec<ServiceCapability>,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("operation `{0}` not supported by service backend `{1}`")]
    Unsupported(String, String),
    #[error("service not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Other(String),
}

impl ServiceError {
    /// A backend op that is scaffolded but not yet implemented. The generated
    /// plugin skeletons return this until the service-specific logic lands.
    pub fn unimplemented(op: &str) -> Self {
        ServiceError::Other(format!("`{op}` not yet implemented"))
    }
}

// ── Adapter trait ────────────────────────────────────────────────────────────

/// One service integration. A backend plugin implements this; the generic
/// `service.*` tools drive it. Lifecycle methods default to "unimplemented" so
/// a scaffold compiles and a partial backend only overrides what it supports.
///
/// A backend never places its own workload — [`workload_spec`](Self::workload_spec)
/// returns a runtime-agnostic [`WorkloadSpec`] and the `service.deploy` tool
/// hands it to a deploy target. The backend owns only what is service-specific:
/// the spec, plus `backup`/`restore`/`configure`/`status`.
pub trait ServiceBackend: Send + Sync {
    /// Provider name (`"audiobookshelf"`). Unique across the registry.
    fn provider(&self) -> &str;

    /// Runtimes this software supports being placed on. Empty for device/host
    /// services (mikrotik, a host UPS daemon) that aren't container/VM workloads.
    fn runtimes(&self) -> Vec<Runtime>;

    /// Default service port.
    fn default_port(&self) -> u16;

    /// Lifecycle ops this backend actually implements. Defaults to the full set;
    /// override to narrow (e.g. a device-only backend drops `Deploy`).
    fn capabilities(&self) -> Vec<ServiceCapability> {
        vec![
            ServiceCapability::Deploy,
            ServiceCapability::Backup,
            ServiceCapability::Restore,
            ServiceCapability::Configure,
            ServiceCapability::Status,
        ]
    }

    /// Non-secret endpoint string for display. Empty when the provider has no
    /// single fixed endpoint.
    fn endpoint(&self) -> String {
        String::new()
    }

    /// In-workload paths holding config/data that `backup`/`restore` snapshot
    /// (e.g. `["/config"]`). This is the ONLY thing a backend declares for
    /// backup — the generic, runtime-agnostic `backup`/`restore` below tar these
    /// paths whether the instance is a container, LXC, or VM. Empty = the
    /// generic backup is unavailable and a backend must override `backup`.
    fn data_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// This backend's minimal, restore-sufficient state as the shared unit-surface
    /// [`BackupSpec`]. Defaults to a paths spec over [`Self::data_paths`], so a
    /// backend that declares `data_paths` also declares a coherent spec for free;
    /// a backend with a non-filesystem backup (DB dump) overrides this to describe
    /// what it actually captures. This is the service-domain half of wiring
    /// `BackupSpec` through every managed unit (docker/proxmox declare theirs on
    /// the `KindDeclaration`).
    fn backup_spec(&self) -> contract::backup::BackupSpec {
        contract::backup::BackupSpec::paths(self.data_paths())
    }

    /// Self-report for `service.list` — never restated in a hand-written literal.
    fn descriptor(&self) -> ServiceProvider {
        ServiceProvider {
            name: self.provider().to_string(),
            runtimes: self.runtimes(),
            default_port: self.default_port(),
            endpoint: self.endpoint(),
            capabilities: self.capabilities(),
        }
    }

    /// Produce the runtime-agnostic workload descriptor for placement on a
    /// deploy target. This is the ONLY deploy-related thing a backend does — the
    /// `service.deploy` tool resolves a target and calls its `launch(spec)`.
    fn workload_spec<'a>(
        &'a self,
        _runtime: Runtime,
        _ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("workload_spec")) })
    }

    /// Generic, runtime-agnostic backup. Tars the backend's `data_paths` inside
    /// the running instance regardless of whether it is a container, LXC, or VM.
    /// A backend gets working backup for free by declaring `data_paths` — it
    /// overrides this only for non-filesystem backup (e.g. a DB dump).
    ///
    /// In-process only: the generic implementation drives the subprocess-backed
    /// [`BackupMethod`] registry (`tar`/`pbs`), which requires `tokio::process`.
    /// On the thin profile that capability is a compile-time absence, so the
    /// default degrades to `unimplemented` — a thin backend that needs backup
    /// overrides this, and the daemon (in-process) provides the generic path.
    #[cfg(feature = "in-process")]
    fn backup<'a>(
        &'a self,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        let provider = self.provider().to_string();
        let paths = self.data_paths();
        let runtime = ep.runtime.or_else(|| self.runtimes().first().copied());
        Box::pin(async move {
            let rt = runtime.ok_or_else(|| {
                ServiceError::Other(format!("{provider}: no runtime to back up against"))
            })?;
            let method = select_method(ep, rt);
            method
                .backup(BackupContext {
                    runtime: rt,
                    endpoint: ep,
                    provider: &provider,
                    data_paths: &paths,
                })
                .await
        })
    }

    /// Thin profile: the subprocess backup capability is not linked, so the
    /// generic implementation is absent. Overriding backends still work.
    #[cfg(not(feature = "in-process"))]
    fn backup<'a>(
        &'a self,
        _ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("backup")) })
    }

    /// Generic, runtime-agnostic restore — inverse of [`backup`](Self::backup).
    /// In-process only, for the same reason as [`backup`](Self::backup).
    #[cfg(feature = "in-process")]
    fn restore<'a>(
        &'a self,
        ep: &'a Endpoint,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        let provider = self.provider().to_string();
        let paths = self.data_paths();
        let runtime = ep.runtime.or_else(|| self.runtimes().first().copied());
        Box::pin(async move {
            let rt = runtime.ok_or_else(|| {
                ServiceError::Other(format!("{provider}: no runtime to restore against"))
            })?;
            let method = select_method(ep, rt);
            method
                .restore(
                    BackupContext {
                        runtime: rt,
                        endpoint: ep,
                        provider: &provider,
                        data_paths: &paths,
                    },
                    from,
                )
                .await
        })
    }

    /// Thin profile: subprocess restore capability absent — see [`backup`](Self::backup).
    #[cfg(not(feature = "in-process"))]
    fn restore<'a>(
        &'a self,
        _ep: &'a Endpoint,
        _from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("restore")) })
    }

    fn configure<'a>(
        &'a self,
        _ep: &'a Endpoint,
        _config: &'a str,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("configure")) })
    }

    fn status<'a>(
        &'a self,
        _ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("status")) })
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn ServiceBackend>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register (or replace, by provider name) a service backend.
pub fn register_backend(backend: Arc<dyn ServiceBackend>) {
    let mut g = GLOBAL.write().expect("service registry poisoned");
    let name = backend.provider().to_string();
    if let Some(slot) = g.iter_mut().find(|b| b.provider() == name) {
        *slot = backend;
    } else {
        g.push(backend);
    }
}

pub fn backends() -> Vec<Arc<dyn ServiceBackend>> {
    GLOBAL.read().expect("service registry poisoned").clone()
}

pub fn backend(name: &str) -> Option<Arc<dyn ServiceBackend>> {
    GLOBAL
        .read()
        .expect("service registry poisoned")
        .iter()
        .find(|b| b.provider() == name)
        .cloned()
}

pub fn deregister_backend(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("service registry poisoned");
    let before = g.len();
    g.retain(|b| b.provider() != name);
    before != g.len()
}

/// Descriptor rows for every registered provider — the `service.list` view.
pub fn providers() -> Vec<ServiceProvider> {
    backends().iter().map(|b| b.descriptor()).collect()
}

// ── Host-side loaded-plugin JSON proxy ───────────────────────────────────────

/// Synchronous thunk a loaded plugin exposes; the proxy offloads it
/// onto `spawn_blocking`. `(op, args_json) -> result_json`.
/// Host-side (in-process) only: drives a *loaded plugin* over the subprocess wire
/// via `spawn_blocking` — a daemon/host concern, gated out on thin.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> Result<String, ServiceError> + Send + Sync + 'static>;

/// Register a backend from a plugin's `BackendDef`, wiring its ops back through
/// `invoke`. `default_port` is the `kind` string, `runtimes` the `runtime` CSV,
/// both raw from the def. Unknown values are rejected at load.
#[cfg(feature = "in-process")]
pub fn register_from_def(
    name: String,
    default_port: &str,
    runtimes_csv: &str,
    endpoint: String,
    capabilities: &[String],
    invoke: InvokeThunk,
) -> Result<(), ServiceError> {
    let default_port: u16 = default_port
        .parse()
        .map_err(|e| ServiceError::Other(format!("bad default_port `{default_port}`: {e}")))?;
    let runtimes = runtimes_csv
        .split(',')
        .filter(|s| !s.is_empty())
        .map(parse_runtime)
        .collect::<Result<Vec<_>, _>>()?;
    let capabilities = capabilities
        .iter()
        .map(|c| ServiceCapability::parse(c))
        .collect::<Result<Vec<_>, _>>()?;
    register_backend(Arc::new(ServiceProxy {
        name,
        runtimes,
        default_port,
        endpoint,
        capabilities,
        invoke,
    }));
    Ok(())
}

/// A [`ServiceBackend`] backed by a subprocess plugin reached over the JSON-proxy
/// wire. Each method serializes args, offloads the sync thunk to
/// `spawn_blocking`, and deserializes the result.
#[cfg(feature = "in-process")]
struct ServiceProxy {
    name: String,
    runtimes: Vec<Runtime>,
    default_port: u16,
    endpoint: String,
    capabilities: Vec<ServiceCapability>,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl ServiceProxy {
    async fn call<A, R>(&self, op: &'static str, args: A) -> Result<R, ServiceError>
    where
        A: Serialize,
        R: serde::de::DeserializeOwned,
    {
        let args_json = serde_json::to_string(&args)
            .map_err(|e| ServiceError::Other(format!("encode `{op}` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
            .await
            .map_err(|e| ServiceError::Transport(format!("`{op}` proxy task failed: {e}")))??;
        serde_json::from_str(&out)
            .map_err(|e| ServiceError::Other(format!("decode `{op}` result: {e}")))
    }
}

#[cfg(feature = "in-process")]
impl ServiceBackend for ServiceProxy {
    fn provider(&self) -> &str {
        &self.name
    }
    fn runtimes(&self) -> Vec<Runtime> {
        self.runtimes.clone()
    }
    fn default_port(&self) -> u16 {
        self.default_port
    }
    fn capabilities(&self) -> Vec<ServiceCapability> {
        self.capabilities.clone()
    }
    fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    fn workload_spec<'a>(
        &'a self,
        runtime: Runtime,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
        Box::pin(self.call(
            "workload_spec",
            RuntimeArgs {
                runtime,
                endpoint: ep.clone(),
            },
        ))
    }

    fn backup<'a>(
        &'a self,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(self.call(
            "backup",
            EndpointArg {
                endpoint: ep.clone(),
            },
        ))
    }

    fn restore<'a>(
        &'a self,
        ep: &'a Endpoint,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(self.call(
            "restore",
            RestoreArgs {
                endpoint: ep.clone(),
                from: from.clone(),
            },
        ))
    }

    fn configure<'a>(
        &'a self,
        ep: &'a Endpoint,
        config: &'a str,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(self.call(
            "configure",
            ConfigureArgs {
                endpoint: ep.clone(),
                config: config.to_string(),
            },
        ))
    }

    fn status<'a>(
        &'a self,
        ep: &'a Endpoint,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(self.call(
            "status",
            EndpointArg {
                endpoint: ep.clone(),
            },
        ))
    }
}

// ── Proxy wire-args ──────────────────────────────────────────────────────────
// Typed args each proxied op serializes across the FFI boundary. Defined (not
// `json!`'d) so both halves deserialize against the same shape.

#[derive(Serialize, Deserialize)]
struct RuntimeArgs {
    runtime: Runtime,
    endpoint: Endpoint,
}

#[derive(Serialize, Deserialize)]
struct EndpointArg {
    endpoint: Endpoint,
}

#[derive(Serialize, Deserialize)]
struct RestoreArgs {
    endpoint: Endpoint,
    from: BackupArtifact,
}

#[derive(Serialize, Deserialize)]
struct ConfigureArgs {
    endpoint: Endpoint,
    config: String,
}

/// Plugin-side inverse of [`ServiceProxy`]: decode a proxied op's JSON args and
/// route it to an in-process [`ServiceBackend`]. A backend plugin's
/// `invoke` is one call to this — never a hand-copied per-op match.
#[allow(clippy::disallowed_types)] // erased-invoke dispatch seam — Value in/out.
pub async fn dispatch_op(
    backend: &dyn ServiceBackend,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    fn enc<T: Serialize>(value: &T) -> Result<serde_json::Value, serde_json::Value> {
        serde_json::to_value(value)
            .map_err(|e| serde_json::Value::String(format!("failed to encode result: {e}")))
    }
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> Result<T, serde_json::Value> {
        serde_json::from_value(args)
            .map_err(|e| serde_json::Value::String(format!("invalid `{op}` args: {e}")))
    }
    fn err<E: std::fmt::Display>(e: E) -> serde_json::Value {
        serde_json::Value::String(e.to_string())
    }

    match op {
        "workload_spec" => {
            let a: RuntimeArgs = dec(op, args)?;
            enc(&backend
                .workload_spec(a.runtime, &a.endpoint)
                .await
                .map_err(err)?)
        }
        "backup" => {
            let a: EndpointArg = dec(op, args)?;
            enc(&backend.backup(&a.endpoint).await.map_err(err)?)
        }
        "restore" => {
            let a: RestoreArgs = dec(op, args)?;
            enc(&backend.restore(&a.endpoint, &a.from).await.map_err(err)?)
        }
        "configure" => {
            let a: ConfigureArgs = dec(op, args)?;
            enc(&backend
                .configure(&a.endpoint, &a.config)
                .await
                .map_err(err)?)
        }
        "status" => {
            let a: EndpointArg = dec(op, args)?;
            enc(&backend.status(&a.endpoint).await.map_err(err)?)
        }
        other => Err(serde_json::Value::String(format!(
            "backend has no operation '{other}'"
        ))),
    }
}

// ── Pluggable backup methods (in-process capability) ─────────────────────────
// "service.backup, that's it": the caller never cares about the runtime OR the
// backup tooling. A backend only declares `data_paths`; a pluggable
// `BackupMethod` does the work. Built-ins: `tar` (container/LXC file snapshot)
// and `pbs` (Proxmox Backup Server). A Proxmox LXC/VM with PBS available routes
// to `pbs` automatically. Plugins (e.g. restic/borg) register more methods.
//
// This whole surface drives subprocesses via `tokio::process`, so it is a
// CAPABILITY gated to `in-process` — exactly like `http`/`db`. A thin plugin
// links none of it (compile-time absence, not a runtime panic); it reaches
// backup through a host round-trip, and the trait's thin `backup`/`restore`
// defaults degrade to `unimplemented`.

#[cfg(feature = "in-process")]
const IN_GUEST_TARBALL: &str = "/tmp/orca-backup.tar.gz";

/// Everything a [`BackupMethod`] needs about the instance being backed up.
#[cfg(feature = "in-process")]
pub struct BackupContext<'a> {
    pub runtime: Runtime,
    pub endpoint: &'a Endpoint,
    pub provider: &'a str,
    pub data_paths: &'a [String],
}

/// A pluggable backup implementation. `tar` and `pbs` ship built-in; a plugin
/// registers others (restic, borg, …) via [`register_method`].
#[cfg(feature = "in-process")]
pub trait BackupMethod: Send + Sync {
    fn name(&self) -> &str;
    /// Whether this method can back up the given runtime in the current env
    /// (e.g. `pbs` only when PBS is configured + the runtime is Proxmox-native).
    fn supports(&self, _runtime: Runtime) -> bool {
        true
    }
    fn backup<'a>(
        &'a self,
        ctx: BackupContext<'a>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>>;
    fn restore<'a>(
        &'a self,
        ctx: BackupContext<'a>,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>>;
}

#[cfg(feature = "in-process")]
static METHODS: LazyLock<RwLock<Vec<Arc<dyn BackupMethod>>>> = LazyLock::new(|| {
    RwLock::new(vec![
        Arc::new(TarMethod) as Arc<dyn BackupMethod>,
        Arc::new(PbsMethod),
    ])
});

/// Register a backup method (or replace one with the same name). Plugins call
/// this at load to add restic/borg/etc.
#[cfg(feature = "in-process")]
pub fn register_method(method: Arc<dyn BackupMethod>) {
    let mut g = METHODS.write().expect("backup-method registry poisoned");
    let name = method.name().to_string();
    if let Some(slot) = g.iter_mut().find(|m| m.name() == name) {
        *slot = method;
    } else {
        g.push(method);
    }
}

#[cfg(feature = "in-process")]
pub fn backup_method(name: &str) -> Option<Arc<dyn BackupMethod>> {
    METHODS
        .read()
        .expect("backup-method registry poisoned")
        .iter()
        .find(|m| m.name() == name)
        .cloned()
}

/// Names of every registered backup method.
#[cfg(feature = "in-process")]
pub fn methods() -> Vec<String> {
    METHODS
        .read()
        .expect("backup-method registry poisoned")
        .iter()
        .map(|m| m.name().to_string())
        .collect()
}

/// Is Proxmox Backup Server usable from here? True when `proxmox-backup-client`
/// is on PATH and a repository is configured (`PBS_REPOSITORY`), or a PBS
/// storage is wired for `vzdump`. Cheap env/file probe; no network call.
#[cfg(feature = "in-process")]
pub fn pbs_available() -> bool {
    std::env::var("PBS_REPOSITORY").is_ok() || std::env::var("ORCA_PBS_STORAGE").is_ok()
}

/// Choose the backup method for an instance: an explicit `endpoint.backup_method`
/// wins; otherwise a Proxmox LXC/VM with PBS available routes to `pbs`; else
/// `tar`. Falls back to `tar` if the chosen method isn't registered.
#[cfg(feature = "in-process")]
pub fn select_method(ep: &Endpoint, runtime: Runtime) -> Arc<dyn BackupMethod> {
    if let Some(name) = ep.backup_method.as_deref()
        && let Some(m) = backup_method(name)
    {
        return m;
    }
    let auto = if matches!(runtime, Runtime::Lxc | Runtime::Vm) && pbs_available() {
        "pbs"
    } else {
        "tar"
    };
    backup_method(auto)
        .or_else(|| backup_method("tar"))
        .expect("tar backup method always registered")
}

#[cfg(feature = "in-process")]
fn run_program(program: &str, args: &[String]) -> Command {
    let mut c = Command::new(program);
    c.args(args);
    c
}

/// Run a command to completion, mapping a non-zero exit to a `Transport` error
/// carrying stderr.
#[cfg(feature = "in-process")]
async fn run(program: &str, args: &[String]) -> Result<(), ServiceError> {
    let out = run_program(program, args)
        .output()
        .await
        .map_err(|e| ServiceError::Transport(format!("spawn `{program}`: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ServiceError::Transport(format!(
            "`{program}` failed ({}): {}",
            out.status,
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(feature = "in-process")]
fn stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// `tar` method: snapshot `data_paths` inside the running instance and pull the
/// tarball to the host. Container runtimes use `<bin> exec`/`cp`; LXC uses
/// `pct exec`/`pull`. No generic path for a bare VM.
#[cfg(feature = "in-process")]
struct TarMethod;

#[cfg(feature = "in-process")]
impl TarMethod {
    fn cli(runtime: Runtime) -> Option<&'static str> {
        match runtime {
            Runtime::Docker => Some("docker"),
            Runtime::Podman => Some("podman"),
            Runtime::Lxc => Some("pct"),
            Runtime::Vm => None,
        }
    }
}

#[cfg(feature = "in-process")]
impl BackupMethod for TarMethod {
    fn name(&self) -> &str {
        "tar"
    }
    fn supports(&self, runtime: Runtime) -> bool {
        Self::cli(runtime).is_some()
    }

    fn backup<'a>(
        &'a self,
        ctx: BackupContext<'a>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(async move {
            if ctx.data_paths.is_empty() {
                return Err(ServiceError::Other(format!(
                    "{}: no data_paths — override `backup` for a custom snapshot",
                    ctx.provider
                )));
            }
            let bin = TarMethod::cli(ctx.runtime).ok_or_else(|| {
                ServiceError::Unsupported("backup".into(), format!("tar on {:?}", ctx.runtime))
            })?;
            let stamp = stamp();
            let out_dir = "/var/tmp/orca-backups";
            std::fs::create_dir_all(out_dir)
                .map_err(|e| ServiceError::Other(format!("mkdir {out_dir}: {e}")))?;
            let out_path = format!(
                "{out_dir}/{}-{}-{stamp}.tar.gz",
                ctx.provider, ctx.endpoint.name
            );
            let handle = &ctx.endpoint.name;
            let tar_cmd = format!("tar czf {IN_GUEST_TARBALL} {}", ctx.data_paths.join(" "));

            if ctx.runtime == Runtime::Lxc {
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.clone(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        tar_cmd,
                    ],
                )
                .await?;
                run(
                    bin,
                    &[
                        "pull".into(),
                        handle.clone(),
                        IN_GUEST_TARBALL.into(),
                        out_path.clone(),
                    ],
                )
                .await?;
            } else {
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.clone(),
                        "sh".into(),
                        "-c".into(),
                        tar_cmd,
                    ],
                )
                .await?;
                run(
                    bin,
                    &[
                        "cp".into(),
                        format!("{handle}:{IN_GUEST_TARBALL}"),
                        out_path.clone(),
                    ],
                )
                .await?;
            }

            Ok(BackupArtifact {
                service: ctx.provider.to_string(),
                instance: ctx.endpoint.name.clone(),
                path: out_path,
                timestamp: stamp,
                ..Default::default()
            })
        })
    }

    fn restore<'a>(
        &'a self,
        ctx: BackupContext<'a>,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            let bin = TarMethod::cli(ctx.runtime).ok_or_else(|| {
                ServiceError::Unsupported("restore".into(), format!("tar on {:?}", ctx.runtime))
            })?;
            let handle = &ctx.endpoint.name;
            let extract = format!("tar xzf {IN_GUEST_TARBALL} -C /");
            if ctx.runtime == Runtime::Lxc {
                run(
                    bin,
                    &[
                        "push".into(),
                        handle.clone(),
                        from.path.clone(),
                        IN_GUEST_TARBALL.into(),
                    ],
                )
                .await?;
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.clone(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        extract,
                    ],
                )
                .await?;
            } else {
                run(
                    bin,
                    &[
                        "cp".into(),
                        from.path.clone(),
                        format!("{handle}:{IN_GUEST_TARBALL}"),
                    ],
                )
                .await?;
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.clone(),
                        "sh".into(),
                        "-c".into(),
                        extract,
                    ],
                )
                .await?;
            }
            Ok(())
        })
    }
}

/// `pbs` method: Proxmox Backup Server. A Proxmox **LXC/VM** is backed up
/// natively with `vzdump --storage <pbs>` (whole-guest, to the PBS-backed
/// storage). A container/host filesystem is backed up with
/// `proxmox-backup-client` (file-level, using `PBS_REPOSITORY`/`PBS_PASSWORD`).
/// Selected automatically for Proxmox guests when [`pbs_available`] is true.
#[cfg(feature = "in-process")]
struct PbsMethod;

#[cfg(feature = "in-process")]
impl PbsMethod {
    fn pbs_storage() -> String {
        std::env::var("ORCA_PBS_STORAGE").unwrap_or_else(|_| "pbs".to_string())
    }
}

#[cfg(feature = "in-process")]
impl BackupMethod for PbsMethod {
    fn name(&self) -> &str {
        "pbs"
    }
    fn supports(&self, _runtime: Runtime) -> bool {
        pbs_available()
    }

    fn backup<'a>(
        &'a self,
        ctx: BackupContext<'a>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(async move {
            let stamp = stamp();
            match ctx.runtime {
                // Whole-guest backup to a PBS-backed storage on the Proxmox node.
                Runtime::Lxc | Runtime::Vm => {
                    let storage = PbsMethod::pbs_storage();
                    run(
                        "vzdump",
                        &[
                            ctx.endpoint.name.clone(), // vmid
                            "--storage".into(),
                            storage.clone(),
                            "--mode".into(),
                            "snapshot".into(),
                        ],
                    )
                    .await?;
                    Ok(BackupArtifact {
                        service: ctx.provider.to_string(),
                        instance: ctx.endpoint.name.clone(),
                        path: format!("pbs:{storage}/{}", ctx.endpoint.name),
                        timestamp: stamp,
                        ..Default::default()
                    })
                }
                // File-level backup of a container/host via proxmox-backup-client.
                Runtime::Docker | Runtime::Podman => {
                    if ctx.data_paths.is_empty() {
                        return Err(ServiceError::Other(format!(
                            "{}: no data_paths for pbs file backup",
                            ctx.provider
                        )));
                    }
                    let mut args = vec!["backup".to_string()];
                    for p in ctx.data_paths {
                        let archive = p.trim_matches('/').replace('/', "_");
                        args.push(format!("{archive}.pxar:{p}"));
                    }
                    run("proxmox-backup-client", &args).await?;
                    Ok(BackupArtifact {
                        service: ctx.provider.to_string(),
                        instance: ctx.endpoint.name.clone(),
                        path: format!("pbs:{}", ctx.endpoint.name),
                        timestamp: stamp,
                        ..Default::default()
                    })
                }
            }
        })
    }

    fn restore<'a>(
        &'a self,
        ctx: BackupContext<'a>,
        _from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            // PBS restore is destructive + guest-specific (pct restore / qmrestore
            // / proxmox-backup-client restore); wire per-runtime intentionally
            // rather than guess a target vmid.
            Err(ServiceError::Other(format!(
                "pbs restore for {} ({:?}) must be performed explicitly via pct/qm/proxmox-backup-client restore",
                ctx.provider, ctx.runtime
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        name: String,
    }

    impl ServiceBackend for Fake {
        fn provider(&self) -> &str {
            &self.name
        }
        fn runtimes(&self) -> Vec<Runtime> {
            vec![Runtime::Docker, Runtime::Lxc]
        }
        fn default_port(&self) -> u16 {
            8080
        }
        fn status<'a>(
            &'a self,
            _ep: &'a Endpoint,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async move {
                Ok(ServiceStatus {
                    healthy: true,
                    detail: "ok".into(),
                    ..Default::default()
                })
            })
        }
    }

    #[test]
    fn backup_spec_defaults_to_paths_over_data_paths() {
        use contract::backup::BackupStrategy;
        // A backend with no data_paths yields an empty paths spec.
        let bare = Fake { name: "f".into() };
        let s = bare.backup_spec();
        assert!(s.include.is_empty());
        assert_eq!(s.strategies, vec![BackupStrategy::Paths]);

        // A backend that declares data_paths declares a coherent spec for free.
        struct WithData;
        impl ServiceBackend for WithData {
            fn provider(&self) -> &str {
                "withdata"
            }
            fn runtimes(&self) -> Vec<Runtime> {
                vec![Runtime::Docker]
            }
            fn default_port(&self) -> u16 {
                80
            }
            fn data_paths(&self) -> Vec<String> {
                vec!["/config".into(), "/data".into()]
            }
        }
        let s = WithData.backup_spec();
        assert_eq!(s.include, vec!["/config".to_string(), "/data".to_string()]);
        assert_eq!(s.strategies, vec![BackupStrategy::Paths]);
    }

    // Owned by the in-process profile: `#[tokio::test]` needs the reactor. The
    // plain `#[test]`s below stay on both profiles (they touch no tokio).
    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_routes_status_and_unimplemented() {
        let f = Fake {
            name: "fake".into(),
        };
        let out = dispatch_op(
            &f,
            "status",
            serde_json::json!({"endpoint":{"name":"x","base_url":"http://h"}}),
        )
        .await
        .expect("status ok");
        assert_eq!(out["healthy"], serde_json::json!(true));

        // workload_spec uses the trait default → unimplemented error value.
        let err = dispatch_op(
            &f,
            "workload_spec",
            serde_json::json!({"runtime":"docker","endpoint":{"name":"x","base_url":""}}),
        )
        .await
        .expect_err("workload_spec unimplemented");
        let err = err.to_string();
        assert!(err.contains("not yet implemented"), "got: {err}");

        let err = dispatch_op(&f, "frobnicate", serde_json::json!({}))
            .await
            .expect_err("unknown op")
            .to_string();
        assert!(err.contains("no operation"), "got: {err}");
    }

    #[test]
    fn registry_replaces_by_name() {
        register_backend(Arc::new(Fake { name: "dup".into() }));
        register_backend(Arc::new(Fake { name: "dup".into() }));
        assert_eq!(
            backends().iter().filter(|b| b.provider() == "dup").count(),
            1
        );
        assert!(deregister_backend("dup"));
    }

    #[test]
    fn runtime_roundtrips_via_deploy_target_mapping() {
        for r in [Runtime::Docker, Runtime::Podman, Runtime::Lxc, Runtime::Vm] {
            assert_eq!(parse_runtime(&runtime_str(r)).unwrap(), r);
        }
    }

    #[test]
    fn parse_runtime_rejects_unknown() {
        let e = parse_runtime("banana").expect_err("unknown runtime");
        assert!(e.to_string().contains("unknown runtime `banana`"), "{e}");
    }

    // ── ServiceCapability ────────────────────────────────────────────────

    #[test]
    fn service_capability_as_str_and_parse_round_trip() {
        for c in [
            ServiceCapability::Deploy,
            ServiceCapability::Backup,
            ServiceCapability::Restore,
            ServiceCapability::Configure,
            ServiceCapability::Status,
        ] {
            assert_eq!(ServiceCapability::parse(c.as_str()).unwrap(), c);
        }
    }

    #[test]
    fn service_capability_parse_rejects_unknown() {
        let e = ServiceCapability::parse("frobnicate").expect_err("unknown cap");
        assert!(
            e.to_string()
                .contains("unknown service capability `frobnicate`"),
            "{e}"
        );
    }

    // ── ServiceError ─────────────────────────────────────────────────────

    #[test]
    fn service_error_unimplemented_message() {
        let e = ServiceError::unimplemented("deploy");
        assert_eq!(e.to_string(), "`deploy` not yet implemented");
    }

    #[test]
    fn service_error_display_variants() {
        assert_eq!(
            ServiceError::Transport("boom".into()).to_string(),
            "transport error: boom"
        );
        assert_eq!(
            ServiceError::Unsupported("backup".into(), "abs".into()).to_string(),
            "operation `backup` not supported by service backend `abs`"
        );
        assert_eq!(
            ServiceError::NotFound("abs".into()).to_string(),
            "service not found: abs"
        );
        assert_eq!(ServiceError::Other("x".into()).to_string(), "x");
    }

    // ── Trait defaults + descriptor ──────────────────────────────────────

    #[test]
    fn default_capabilities_is_full_set() {
        let f = Fake { name: "f".into() };
        assert_eq!(
            f.capabilities(),
            vec![
                ServiceCapability::Deploy,
                ServiceCapability::Backup,
                ServiceCapability::Restore,
                ServiceCapability::Configure,
                ServiceCapability::Status,
            ]
        );
        // Default endpoint is empty for a backend that doesn't override it.
        assert_eq!(f.endpoint(), "");
        assert!(f.data_paths().is_empty());
    }

    #[test]
    fn descriptor_composes_backend_self_report() {
        let f = Fake { name: "abs".into() };
        let d = f.descriptor();
        assert_eq!(d.name, "abs");
        assert_eq!(d.runtimes, vec![Runtime::Docker, Runtime::Lxc]);
        assert_eq!(d.default_port, 8080);
        assert_eq!(d.endpoint, "");
        assert_eq!(d.capabilities.len(), 5);
    }

    // ── Model serde ──────────────────────────────────────────────────────

    #[test]
    fn endpoint_round_trips_and_defaults_optional_fields() {
        let ep: Endpoint =
            serde_json::from_value(serde_json::json!({"name":"x","base_url":"http://h"}))
                .expect("minimal endpoint decodes");
        assert_eq!(ep.name, "x");
        assert_eq!(ep.target_host, "");
        assert_eq!(ep.runtime, None);
        assert_eq!(ep.backup_method, None);
        assert_eq!(ep.token, "");

        let full = Endpoint {
            name: "n".into(),
            base_url: "u".into(),
            target_host: "host".into(),
            runtime: Some(Runtime::Lxc),
            backup_method: Some("tar".into()),
            token: "t".into(),
        };
        let s = serde_json::to_string(&full).unwrap();
        let back: Endpoint = serde_json::from_str(&s).unwrap();
        assert_eq!(back.runtime, Some(Runtime::Lxc));
        assert_eq!(back.backup_method.as_deref(), Some("tar"));
        assert_eq!(back.token, "t");
    }

    #[test]
    fn service_status_default_and_info_none_tag() {
        let st = ServiceStatus::default();
        assert!(!st.healthy);
        assert_eq!(st.detail, "");
        let v = serde_json::to_value(&st.info).unwrap();
        assert_eq!(v, serde_json::json!({"kind":"none"}));
        assert!(matches!(ServiceInfo::default(), ServiceInfo::None));
    }

    #[test]
    fn backup_artifact_defaults_size_and_checksum() {
        let a: BackupArtifact = serde_json::from_value(serde_json::json!({
            "service":"abs","instance":"main","path":"/p","timestamp":"20250101-000000"
        }))
        .expect("artifact decodes");
        assert_eq!(a.size_bytes, 0);
        assert_eq!(a.checksum, "");
    }

    // ── Registry ─────────────────────────────────────────────────────────

    #[test]
    fn backend_lookup_and_providers_reflect_registry() {
        register_backend(Arc::new(Fake {
            name: "reg-look".into(),
        }));
        let b = backend("reg-look").expect("found");
        assert_eq!(b.provider(), "reg-look");
        assert!(backend("does-not-exist").is_none());
        assert!(providers().iter().any(|p| p.name == "reg-look"));
        assert!(deregister_backend("reg-look"));
        assert!(!deregister_backend("reg-look"));
    }

    // ── dispatch_op: full op coverage ────────────────────────────────────

    // A backend that implements the lifecycle ops so dispatch's success paths
    // (workload_spec/configure/backup) are exercised, not just the defaults.
    struct FullBackend;
    impl ServiceBackend for FullBackend {
        fn provider(&self) -> &str {
            "full"
        }
        fn runtimes(&self) -> Vec<Runtime> {
            vec![Runtime::Docker]
        }
        fn default_port(&self) -> u16 {
            9000
        }
        fn workload_spec<'a>(
            &'a self,
            runtime: Runtime,
            ep: &'a Endpoint,
        ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
            let name = format!("{}-{}", ep.name, runtime_str(runtime));
            Box::pin(async move {
                Ok(WorkloadSpec {
                    name,
                    ..Default::default()
                })
            })
        }
        fn configure<'a>(
            &'a self,
            _ep: &'a Endpoint,
            config: &'a str,
        ) -> BoxFuture<'a, Result<(), ServiceError>> {
            Box::pin(async move {
                if config.is_empty() {
                    Err(ServiceError::Other("empty config".into()))
                } else {
                    Ok(())
                }
            })
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_routes_workload_spec_success() {
        let b = FullBackend;
        let out = dispatch_op(
            &b,
            "workload_spec",
            serde_json::json!({"runtime":"docker","endpoint":{"name":"x","base_url":""}}),
        )
        .await
        .expect("workload_spec ok");
        assert_eq!(out["name"], serde_json::json!("x-docker"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_routes_configure_and_surfaces_backend_error() {
        let b = FullBackend;
        // Success path.
        dispatch_op(
            &b,
            "configure",
            serde_json::json!({"endpoint":{"name":"x","base_url":""},"config":"yaml"}),
        )
        .await
        .expect("configure ok");

        // Backend-returned error is surfaced as an error value.
        let e = dispatch_op(
            &b,
            "configure",
            serde_json::json!({"endpoint":{"name":"x","base_url":""},"config":""}),
        )
        .await
        .expect_err("empty config errors")
        .to_string();
        assert!(e.contains("empty config"), "{e}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_backup_and_restore_default_to_unimplemented() {
        let f = Fake {
            name: "fake".into(),
        };
        let e = dispatch_op(
            &f,
            "backup",
            serde_json::json!({"endpoint":{"name":"x","base_url":""}}),
        )
        .await
        .expect_err("backup needs runtime or unimpl");
        // Fake declares runtimes, so the generic backup proceeds to method
        // selection; either way the op errored deterministically (no panic).
        assert!(!e.to_string().is_empty());

        let e = dispatch_op(
            &f,
            "restore",
            serde_json::json!({"endpoint":{"name":"x","base_url":""},"from":{"service":"s","instance":"i","path":"/p","timestamp":"t"}}),
        )
        .await
        .expect_err("restore errors without a real instance");
        assert!(!e.to_string().is_empty());
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn dispatch_reports_decode_error_on_malformed_args() {
        let f = Fake {
            name: "fake".into(),
        };
        let e = dispatch_op(&f, "status", serde_json::json!("not an object"))
            .await
            .expect_err("bad args")
            .to_string();
        assert!(e.contains("invalid `status` args"), "{e}");
    }

    // ── register_from_def proxy path ─────────────────────────────────────

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn register_from_def_wires_proxy_ops_and_deregisters() {
        let thunk: InvokeThunk = Arc::new(|op: &str, args_json: String| match op {
            "status" => {
                let _a: EndpointArg = serde_json::from_str(&args_json).unwrap();
                let st = ServiceStatus {
                    healthy: true,
                    detail: "proxied".into(),
                    ..Default::default()
                };
                Ok(serde_json::to_string(&st).unwrap())
            }
            "configure" => {
                let a: ConfigureArgs = serde_json::from_str(&args_json).unwrap();
                assert_eq!(a.config, "cfg");
                Ok("null".to_string())
            }
            other => Err(ServiceError::Other(format!("unexpected {other}"))),
        });

        register_from_def(
            "proxy-abs".into(),
            "8080",
            "docker,lxc",
            "http://abs".into(),
            &["status".into(), "configure".into()],
            thunk,
        )
        .expect("def registers");

        let b = backend("proxy-abs").expect("registered");
        assert_eq!(b.default_port(), 8080);
        assert_eq!(b.runtimes(), vec![Runtime::Docker, Runtime::Lxc]);
        assert_eq!(b.endpoint(), "http://abs");
        assert_eq!(
            b.capabilities(),
            vec![ServiceCapability::Status, ServiceCapability::Configure]
        );

        let ep = Endpoint {
            name: "main".into(),
            base_url: "http://abs".into(),
            ..Default::default()
        };
        let st = b.status(&ep).await.expect("proxied status");
        assert!(st.healthy);
        assert_eq!(st.detail, "proxied");
        b.configure(&ep, "cfg").await.expect("proxied configure");

        assert!(deregister_backend("proxy-abs"));
        assert!(backend("proxy-abs").is_none());
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn register_from_def_rejects_bad_port_runtime_and_capability() {
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("null".into()));
        // Bad port.
        assert!(
            register_from_def(
                "bad-port".into(),
                "not-a-number",
                "docker",
                "e".into(),
                &[],
                thunk.clone()
            )
            .is_err()
        );
        // Bad runtime.
        assert!(
            register_from_def(
                "bad-rt".into(),
                "80",
                "docker,banana",
                "e".into(),
                &[],
                thunk.clone()
            )
            .is_err()
        );
        // Bad capability.
        assert!(
            register_from_def(
                "bad-cap".into(),
                "80",
                "docker",
                "e".into(),
                &["fly".into()],
                thunk
            )
            .is_err()
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn register_from_def_ignores_empty_runtime_csv_entries() {
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("null".into()));
        register_from_def(
            "empty-csv".into(),
            "80",
            // trailing/empty segments must be filtered, not parsed.
            "docker,,",
            "e".into(),
            &[],
            thunk,
        )
        .expect("empty csv segments filtered");
        let b = backend("empty-csv").expect("registered");
        assert_eq!(b.runtimes(), vec![Runtime::Docker]);
        deregister_backend("empty-csv");
    }

    // ── Backup method registry ───────────────────────────────────────────

    #[cfg(feature = "in-process")]
    #[test]
    fn builtin_backup_methods_present() {
        let names = methods();
        assert!(names.iter().any(|n| n == "tar"), "{names:?}");
        assert!(names.iter().any(|n| n == "pbs"), "{names:?}");
        assert!(backup_method("tar").is_some());
        assert!(backup_method("pbs").is_some());
        assert!(backup_method("nonexistent").is_none());
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn tar_method_cli_mapping_and_supports() {
        assert_eq!(TarMethod::cli(Runtime::Docker), Some("docker"));
        assert_eq!(TarMethod::cli(Runtime::Podman), Some("podman"));
        assert_eq!(TarMethod::cli(Runtime::Lxc), Some("pct"));
        assert_eq!(TarMethod::cli(Runtime::Vm), None);
        let tar = TarMethod;
        assert!(tar.supports(Runtime::Docker));
        assert!(!tar.supports(Runtime::Vm));
        assert_eq!(tar.name(), "tar");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn pbs_method_name_supports_and_storage() {
        let pbs = PbsMethod;
        assert_eq!(pbs.name(), "pbs");
        // supports() mirrors pbs_available(); both read the same env, so they agree.
        assert_eq!(pbs.supports(Runtime::Lxc), pbs_available());
        // pbs_storage falls back to "pbs" unless ORCA_PBS_STORAGE is set.
        if std::env::var("ORCA_PBS_STORAGE").is_err() {
            assert_eq!(PbsMethod::pbs_storage(), "pbs");
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn pbs_restore_is_intentionally_unsupported() {
        let pbs = PbsMethod;
        let ep = Endpoint {
            name: "100".into(),
            ..Default::default()
        };
        let art = BackupArtifact::default();
        let e = pbs
            .restore(
                BackupContext {
                    runtime: Runtime::Lxc,
                    endpoint: &ep,
                    provider: "abs",
                    data_paths: &[],
                },
                &art,
            )
            .await
            .expect_err("pbs restore is explicit-only");
        assert!(
            e.to_string().contains("must be performed explicitly"),
            "{e}"
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn tar_backup_rejects_empty_data_paths_and_bare_vm() {
        let tar = TarMethod;
        let ep = Endpoint {
            name: "inst".into(),
            ..Default::default()
        };
        // Empty data_paths → clear guidance error, before any subprocess.
        let e = tar
            .backup(BackupContext {
                runtime: Runtime::Docker,
                endpoint: &ep,
                provider: "abs",
                data_paths: &[],
            })
            .await
            .expect_err("no data_paths");
        assert!(e.to_string().contains("no data_paths"), "{e}");

        // A bare VM has no generic tar CLI → Unsupported, before any subprocess.
        let paths = ["/config".to_string()];
        let e = tar
            .backup(BackupContext {
                runtime: Runtime::Vm,
                endpoint: &ep,
                provider: "abs",
                data_paths: &paths,
            })
            .await
            .expect_err("no tar path on bare vm");
        assert!(matches!(e, ServiceError::Unsupported(_, _)), "{e}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn tar_restore_on_bare_vm_is_unsupported() {
        let tar = TarMethod;
        let ep = Endpoint {
            name: "inst".into(),
            ..Default::default()
        };
        let art = BackupArtifact::default();
        let e = tar
            .restore(
                BackupContext {
                    runtime: Runtime::Vm,
                    endpoint: &ep,
                    provider: "abs",
                    data_paths: &[],
                },
                &art,
            )
            .await
            .expect_err("no tar restore on bare vm");
        assert!(matches!(e, ServiceError::Unsupported(_, _)), "{e}");
    }

    // ── select_method ────────────────────────────────────────────────────

    #[cfg(feature = "in-process")]
    #[test]
    fn select_method_honors_explicit_choice() {
        // A registered custom method chosen explicitly by the endpoint wins.
        struct Restic;
        impl BackupMethod for Restic {
            fn name(&self) -> &str {
                "restic-test"
            }
            fn backup<'a>(
                &'a self,
                _ctx: BackupContext<'a>,
            ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
                Box::pin(async move { Ok(BackupArtifact::default()) })
            }
            fn restore<'a>(
                &'a self,
                _ctx: BackupContext<'a>,
                _from: &'a BackupArtifact,
            ) -> BoxFuture<'a, Result<(), ServiceError>> {
                Box::pin(async move { Ok(()) })
            }
        }
        register_method(Arc::new(Restic));
        let ep = Endpoint {
            backup_method: Some("restic-test".into()),
            ..Default::default()
        };
        assert_eq!(select_method(&ep, Runtime::Docker).name(), "restic-test");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn select_method_defaults_to_tar_for_containers() {
        // Docker/Podman are never PBS-native, so they always route to tar.
        let ep = Endpoint::default();
        assert_eq!(select_method(&ep, Runtime::Docker).name(), "tar");
        assert_eq!(select_method(&ep, Runtime::Podman).name(), "tar");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn select_method_falls_back_to_tar_when_choice_unregistered() {
        // An explicit but unknown method name doesn't match; falls to the auto
        // choice (tar for a container).
        let ep = Endpoint {
            backup_method: Some("ghost".into()),
            ..Default::default()
        };
        assert_eq!(select_method(&ep, Runtime::Docker).name(), "tar");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn register_method_replaces_by_name() {
        struct One;
        impl BackupMethod for One {
            fn name(&self) -> &str {
                "dup-method"
            }
            fn backup<'a>(
                &'a self,
                _ctx: BackupContext<'a>,
            ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
                Box::pin(async move { Ok(BackupArtifact::default()) })
            }
            fn restore<'a>(
                &'a self,
                _ctx: BackupContext<'a>,
                _from: &'a BackupArtifact,
            ) -> BoxFuture<'a, Result<(), ServiceError>> {
                Box::pin(async move { Ok(()) })
            }
        }
        register_method(Arc::new(One));
        register_method(Arc::new(One));
        assert_eq!(methods().iter().filter(|n| *n == "dup-method").count(), 1);
    }

    // ── Proxy op success paths (workload_spec / backup / restore) ─────────
    // The existing register_from_def test exercises status + configure. These
    // fill the remaining proxied ops by round-tripping their typed wire-args
    // through a fake thunk (no subprocess, no real plugin).

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn proxy_workload_spec_backup_and_restore_round_trip() {
        let thunk: InvokeThunk = Arc::new(|op: &str, args_json: String| match op {
            "workload_spec" => {
                let a: RuntimeArgs = serde_json::from_str(&args_json).unwrap();
                // Echo the runtime + instance name back through the WorkloadSpec.
                let spec = WorkloadSpec {
                    name: format!("{}-{}", a.endpoint.name, runtime_str(a.runtime)),
                    ..Default::default()
                };
                Ok(serde_json::to_string(&spec).unwrap())
            }
            "backup" => {
                let a: EndpointArg = serde_json::from_str(&args_json).unwrap();
                let art = BackupArtifact {
                    service: "proxied".into(),
                    instance: a.endpoint.name.clone(),
                    path: "/backups/x.tar.gz".into(),
                    timestamp: "20250101-000000".into(),
                    ..Default::default()
                };
                Ok(serde_json::to_string(&art).unwrap())
            }
            "restore" => {
                let a: RestoreArgs = serde_json::from_str(&args_json).unwrap();
                assert_eq!(a.from.path, "/backups/x.tar.gz");
                Ok("null".to_string())
            }
            other => Err(ServiceError::Other(format!("unexpected {other}"))),
        });

        register_from_def(
            "proxy-full".into(),
            "8080",
            "docker",
            "http://x".into(),
            &["deploy".into(), "backup".into(), "restore".into()],
            thunk,
        )
        .expect("def registers");

        let b = backend("proxy-full").expect("registered");
        let ep = Endpoint {
            name: "main".into(),
            ..Default::default()
        };

        let spec = b
            .workload_spec(Runtime::Docker, &ep)
            .await
            .expect("proxied workload_spec");
        assert_eq!(spec.name, "main-docker");

        let art = b.backup(&ep).await.expect("proxied backup");
        assert_eq!(art.service, "proxied");
        assert_eq!(art.instance, "main");
        assert_eq!(art.path, "/backups/x.tar.gz");

        let from = BackupArtifact {
            path: "/backups/x.tar.gz".into(),
            ..Default::default()
        };
        b.restore(&ep, &from).await.expect("proxied restore");

        assert!(deregister_backend("proxy-full"));
    }

    // Proxy decode failure: a thunk that returns non-JSON surfaces a decode
    // error naming the op, rather than panicking.
    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn proxy_reports_decode_error_on_bad_thunk_output() {
        let thunk: InvokeThunk = Arc::new(|_, _| Ok("not json".to_string()));
        register_from_def(
            "proxy-baddec".into(),
            "80",
            "docker",
            String::new(),
            &["status".into()],
            thunk,
        )
        .expect("def registers");
        let b = backend("proxy-baddec").expect("registered");
        let e = b
            .status(&Endpoint::default())
            .await
            .expect_err("bad thunk output");
        assert!(e.to_string().contains("decode `status` result"), "{e}");
        deregister_backend("proxy-baddec");
    }

    // Proxy error propagation: a thunk that returns Err bubbles the backend
    // error through the `??` in `call`.
    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn proxy_propagates_thunk_error() {
        let thunk: InvokeThunk =
            Arc::new(|_, _| Err(ServiceError::Transport("upstream boom".into())));
        register_from_def(
            "proxy-err".into(),
            "80",
            "docker",
            String::new(),
            &["status".into()],
            thunk,
        )
        .expect("def registers");
        let b = backend("proxy-err").expect("registered");
        let e = b
            .status(&Endpoint::default())
            .await
            .expect_err("thunk error");
        assert!(e.to_string().contains("upstream boom"), "{e}");
        deregister_backend("proxy-err");
    }

    // ── PbsMethod file-backup guard ──────────────────────────────────────
    // A container/host pbs file backup with no data_paths errors before any
    // subprocess — the deterministic, no-exec branch of PbsMethod::backup.

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn pbs_file_backup_rejects_empty_data_paths() {
        let pbs = PbsMethod;
        let ep = Endpoint {
            name: "cont".into(),
            ..Default::default()
        };
        for rt in [Runtime::Docker, Runtime::Podman] {
            let e = pbs
                .backup(BackupContext {
                    runtime: rt,
                    endpoint: &ep,
                    provider: "abs",
                    data_paths: &[],
                })
                .await
                .expect_err("no data_paths for pbs file backup");
            assert!(
                e.to_string().contains("no data_paths for pbs file backup"),
                "{e}"
            );
        }
    }

    // ── stamp() ──────────────────────────────────────────────────────────

    #[cfg(feature = "in-process")]
    #[test]
    fn stamp_is_nonempty_numeric() {
        let s = stamp();
        assert!(!s.is_empty());
        assert!(s.chars().all(|c| c.is_ascii_digit()), "{s}");
    }

    // ── Additional serde shapes (assert on serialized strings) ───────────

    #[test]
    fn service_capability_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ServiceCapability::Deploy).unwrap(),
            "\"deploy\""
        );
        assert_eq!(
            serde_json::to_string(&ServiceCapability::Configure).unwrap(),
            "\"configure\""
        );
        let back: ServiceCapability = serde_json::from_str("\"restore\"").unwrap();
        assert_eq!(back, ServiceCapability::Restore);
    }

    #[test]
    fn service_provider_round_trips_via_string() {
        let p = ServiceProvider {
            name: "abs".into(),
            runtimes: vec![Runtime::Docker, Runtime::Lxc],
            default_port: 8080,
            endpoint: "http://abs".into(),
            capabilities: vec![ServiceCapability::Backup, ServiceCapability::Status],
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: ServiceProvider = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "abs");
        assert_eq!(back.runtimes, vec![Runtime::Docker, Runtime::Lxc]);
        assert_eq!(back.default_port, 8080);
        assert_eq!(back.endpoint, "http://abs");
        assert_eq!(
            back.capabilities,
            vec![ServiceCapability::Backup, ServiceCapability::Status]
        );
    }

    #[test]
    fn backup_artifact_serializes_all_fields() {
        let a = BackupArtifact {
            service: "abs".into(),
            instance: "main".into(),
            path: "/p".into(),
            timestamp: "20250101-000000".into(),
            size_bytes: 42,
            checksum: "deadbeef".into(),
        };
        let s = serde_json::to_string(&a).unwrap();
        let back: BackupArtifact = serde_json::from_str(&s).unwrap();
        assert_eq!(back.size_bytes, 42);
        assert_eq!(back.checksum, "deadbeef");
        assert_eq!(back.service, "abs");
        assert_eq!(back.timestamp, "20250101-000000");
    }
}
