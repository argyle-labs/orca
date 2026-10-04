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
// `Mount`/`EnvVar` come with it: they are `WorkloadSpec`'s own field types, so
// without them a backend can name the struct but cannot populate it.
pub use deploy_target::{EnvVar, Mount, Runtime, WorkloadSpec};

// The first-class reachability primitive — the same ordered `Routes` peers and
// plugin endpoints use, re-exported so a backend reasons over `ep.routes`
// without a second import path.
pub use utils::route::{Route, Routes};

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
                "unknown service capability `{other}` — this plugin declares a capability \
             orca {orca} does not implement, which normally means it was built \
             against a newer orca than this host runs. Install a plugin release built \
             for this orca, or update orca first (#605).",
                orca = env!("CARGO_PKG_VERSION")
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

// There is deliberately NO `Endpoint` type here. What used to be one conflated
// three unrelated things: an instance HANDLE (`name`), its ADDRESS (`routes`),
// and backup selection inputs (`runtime`, `backup_method`) — plus a `token` and
// `target_host` that no backend ever read. Every `ServiceBackend` method now
// takes exactly what it uses, so a signature states its own requirements and
// reachability is the ordered `Routes` set and nothing else (#615).
// `Routes::primary_url` / `Routes::publish_port` carry the two conveniences the
// old type provided.

/// A backup artifact produced by [`ServiceBackend::backup`], restorable via
/// [`ServiceBackend::restore`]. The path is on the deploy target's filesystem.
// Sent daemon -> plugin on restore and persisted as a backup sidecar. Wire
// stays snake_case until every service plugin is re-released. camelCase is
// accepted.
#[derive::camel_aliases]
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
#[derive::snake_aliases]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
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
        _instance: &'a str,
        _routes: &'a Routes,
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
        instance: &'a str,
        runtime: Option<Runtime>,
        method_name: Option<&'a str>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        let provider = self.provider().to_string();
        // From the SPEC, not `data_paths()` directly: the spec is where
        // `exclude` lives, and building the context from the bare path list
        // is what silently discarded it (#613).
        let spec = self.backup_spec();
        let paths = if spec.include.is_empty() {
            self.data_paths()
        } else {
            spec.include.clone()
        };
        let runtime = runtime.or_else(|| self.runtimes().first().copied());
        Box::pin(async move {
            let rt = runtime.ok_or_else(|| {
                ServiceError::Other(format!("{provider}: no runtime to back up against"))
            })?;
            let method = select_method(method_name, rt);
            method
                .backup(BackupContext {
                    runtime: rt,
                    instance,
                    provider: &provider,
                    data_paths: &paths,
                    exclude: &spec.exclude,
                })
                .await
        })
    }

    /// Thin profile: the subprocess backup capability is not linked, so the
    /// generic implementation is absent. Overriding backends still work.
    #[cfg(not(feature = "in-process"))]
    fn backup<'a>(
        &'a self,
        _instance: &'a str,
        _runtime: Option<Runtime>,
        _method_name: Option<&'a str>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("backup")) })
    }

    /// Generic, runtime-agnostic restore — inverse of [`backup`](Self::backup).
    /// In-process only, for the same reason as [`backup`](Self::backup).
    #[cfg(feature = "in-process")]
    fn restore<'a>(
        &'a self,
        instance: &'a str,
        runtime: Option<Runtime>,
        method_name: Option<&'a str>,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        let provider = self.provider().to_string();
        let spec = self.backup_spec();
        let paths = if spec.include.is_empty() {
            self.data_paths()
        } else {
            spec.include.clone()
        };
        let runtime = runtime.or_else(|| self.runtimes().first().copied());
        Box::pin(async move {
            let rt = runtime.ok_or_else(|| {
                ServiceError::Other(format!("{provider}: no runtime to restore against"))
            })?;
            let method = select_method(method_name, rt);
            method
                .restore(
                    BackupContext {
                        runtime: rt,
                        instance,
                        provider: &provider,
                        data_paths: &paths,
                        exclude: &spec.exclude,
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
        _instance: &'a str,
        _runtime: Option<Runtime>,
        _method_name: Option<&'a str>,
        _from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("restore")) })
    }

    fn configure<'a>(
        &'a self,
        _instance: &'a str,
        _routes: &'a Routes,
        _config: &'a str,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move { Err(ServiceError::unimplemented("configure")) })
    }

    fn status<'a>(
        &'a self,
        _instance: &'a str,
        _routes: &'a Routes,
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
        instance: &'a str,
        routes: &'a Routes,
    ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
        Box::pin(self.call(
            "workload_spec",
            RuntimeArgs {
                runtime,
                instance: instance.to_string(),
                routes: routes.clone(),
            },
        ))
    }

    fn backup<'a>(
        &'a self,
        instance: &'a str,
        runtime: Option<Runtime>,
        method_name: Option<&'a str>,
    ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
        Box::pin(self.call(
            "backup",
            BackupArgs {
                instance: instance.to_string(),
                runtime,
                method: method_name.map(str::to_string),
            },
        ))
    }

    fn restore<'a>(
        &'a self,
        instance: &'a str,
        runtime: Option<Runtime>,
        method_name: Option<&'a str>,
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(self.call(
            "restore",
            RestoreArgs {
                instance: instance.to_string(),
                runtime,
                method: method_name.map(str::to_string),
                from: from.clone(),
            },
        ))
    }

    fn configure<'a>(
        &'a self,
        instance: &'a str,
        routes: &'a Routes,
        config: &'a str,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(self.call(
            "configure",
            ConfigureArgs {
                instance: instance.to_string(),
                routes: routes.clone(),
                config: config.to_string(),
            },
        ))
    }

    fn status<'a>(
        &'a self,
        instance: &'a str,
        routes: &'a Routes,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(self.call(
            "status",
            ReachArgs {
                instance: instance.to_string(),
                routes: routes.clone(),
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
    instance: String,
    routes: Routes,
}

/// Ops that REACH the instance carry its handle and its address, nothing else.
#[derive(Serialize, Deserialize)]
struct ReachArgs {
    instance: String,
    routes: Routes,
}

/// Ops that act on the instance's STORAGE carry no routes — a backup does not
/// dial the service, it operates on the runtime handle.
#[derive(Serialize, Deserialize)]
struct BackupArgs {
    instance: String,
    runtime: Option<Runtime>,
    method: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RestoreArgs {
    instance: String,
    runtime: Option<Runtime>,
    method: Option<String>,
    from: BackupArtifact,
}

#[derive(Serialize, Deserialize)]
struct ConfigureArgs {
    instance: String,
    routes: Routes,
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
                .workload_spec(a.runtime, &a.instance, &a.routes)
                .await
                .map_err(err)?)
        }
        "backup" => {
            let a: BackupArgs = dec(op, args)?;
            enc(&backend
                .backup(&a.instance, a.runtime, a.method.as_deref())
                .await
                .map_err(err)?)
        }
        "restore" => {
            let a: RestoreArgs = dec(op, args)?;
            enc(&backend
                .restore(&a.instance, a.runtime, a.method.as_deref(), &a.from)
                .await
                .map_err(err)?)
        }
        "configure" => {
            let a: ConfigureArgs = dec(op, args)?;
            enc(&backend
                .configure(&a.instance, &a.routes, &a.config)
                .await
                .map_err(err)?)
        }
        "status" => {
            let a: ReachArgs = dec(op, args)?;
            enc(&backend.status(&a.instance, &a.routes).await.map_err(err)?)
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
    /// The instance HANDLE: the container name for docker/podman, the `vmid`
    /// for lxc. This is the only thing a backup method ever read off the old
    /// `Endpoint`.
    pub instance: &'a str,
    pub provider: &'a str,
    pub data_paths: &'a [String],
    /// Sub-paths under `data_paths` that are NOT state: caches, thumbnails,
    /// regenerable models, logs, `.git` trees that duplicate a remote.
    ///
    /// Carried here because the contract has always had it
    /// (`BackupSpec::exclude`) and the methods never saw it — `BackupContext`
    /// was built from `data_paths()` alone, so the exclusions were dropped one
    /// layer above every method that could have honored them. That omission is
    /// most of the size problem in #613: radarr 1.9G→145M, calibre-web
    /// 2.2G→10M, willow appdata 18G→1G are all exclusions, not compression.
    pub exclude: &'a [String],
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
pub fn select_method(method_name: Option<&str>, runtime: Runtime) -> Arc<dyn BackupMethod> {
    if let Some(name) = method_name
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

    /// Single-quote a path for the `sh -c` the tar runs inside.
    ///
    /// These paths come from a backend's declared spec and are interpolated
    /// into a shell string. A path containing a space silently split into two
    /// arguments before this — tarring the wrong things and skipping the right
    /// ones, with a zero exit code either way.
    fn shell_quote(p: &str) -> String {
        format!("'{}'", p.replace('\'', r#"'\''"#))
    }

    /// Unpack the backup tarball into a temporary directory on the HOST.
    ///
    /// The extract has to happen somewhere a stopped container can be fed
    /// from, and `<bin> exec` is not available once the unit is down. The
    /// directory is removed when the returned handle drops, including on the
    /// error paths — a restore that fails should not also leave a full copy of
    /// the unit's data in the host's temp space.
    ///
    /// Requires `tar` on the HOST. The backup side only ever needed tar inside
    /// the guest, so this is a new requirement and is reported as a clear
    /// refusal rather than a confusing spawn failure.
    async fn stage(tarball: &str) -> Result<tempfile::TempDir, ServiceError> {
        if utils::path::which("tar").is_none() {
            return Err(ServiceError::Unsupported(
                "restore".to_string(),
                "`tar` is not installed on this host. A quiesced container restore \
                 unpacks host-side, because the extract cannot run inside a stopped \
                 container."
                    .to_string(),
            ));
        }
        let dir = tempfile::tempdir()
            .map_err(|e| ServiceError::Other(format!("staging dir for restore: {e}")))?;
        run(
            "tar",
            &[
                "xzf".to_string(),
                tarball.to_string(),
                "-C".to_string(),
                dir.path().display().to_string(),
            ],
        )
        .await
        // The commonest cause is no room for the UNCOMPRESSED payload, which
        // tar reports as a write error naming neither the cause nor the
        // filesystem. Say where it was unpacking.
        .map_err(|e| {
            ServiceError::Other(format!(
                "unpacking `{tarball}` into `{}` failed: {e}. If the host's temp \
                 filesystem is short of room for the uncompressed payload, point \
                 TMPDIR somewhere with space.",
                dir.path().display()
            ))
        })?;
        Ok(dir)
    }

    /// The in-guest `tar` command line.
    ///
    /// Pure, so the exclusions and the quoting are assertable. `--exclude`
    /// precedes the path operands because GNU tar applies it to operands that
    /// FOLLOW it; trailing excludes parse fine and match nothing.
    fn tar_cmd(paths: &[String], exclude: &[String]) -> String {
        let mut cmd = format!("tar czf {IN_GUEST_TARBALL}");
        for e in exclude.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            cmd.push_str(" --exclude=");
            cmd.push_str(&Self::shell_quote(e));
        }
        for p in paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            cmd.push(' ');
            cmd.push_str(&Self::shell_quote(p));
        }
        cmd
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
            let out_path = format!("{out_dir}/{}-{}-{stamp}.tar.gz", ctx.provider, ctx.instance);
            let handle = &ctx.instance;
            let tar_cmd = TarMethod::tar_cmd(ctx.data_paths, ctx.exclude);

            if ctx.runtime == Runtime::Lxc {
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.to_string(),
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
                        handle.to_string(),
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
                        handle.to_string(),
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
                instance: ctx.instance.to_string(),
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
            let handle = &ctx.instance;
            if ctx.runtime == Runtime::Lxc {
                // An LXC necessarily stays UP: `pct push` and `pct exec` both
                // require a running container, so there is no quiesced form of
                // this transport. Restoring into a live guest risks the same
                // write-back corruption as everywhere else — the honest thing
                // is that this path cannot avoid it, not that it is safe.
                let extract = format!("tar xzf {IN_GUEST_TARBALL} -C /");
                run(
                    bin,
                    &[
                        "push".into(),
                        handle.to_string(),
                        from.path.clone(),
                        IN_GUEST_TARBALL.into(),
                    ],
                )
                .await?;
                run(
                    bin,
                    &[
                        "exec".into(),
                        handle.to_string(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        extract,
                    ],
                )
                .await?;
                return Ok(());
            }

            // Container runtimes CAN be quiesced, so they are. The old path
            // `cp`-ed the tarball in and `exec`-ed tar INSIDE a running unit:
            // the service wrote back over what had just been restored from its
            // own in-memory state, and the restore reported success either way
            // (#676).
            //
            // `exec` cannot run in a stopped container, so the extract moves
            // HOST-side and the contents are copied in — both of which work on
            // a stopped container.
            let staging = TarMethod::stage(&from.path).await?;
            quiesce(bin, handle).await?;
            let restored = run(
                bin,
                &[
                    "cp".into(),
                    // The trailing `/.` copies the CONTENTS of the directory,
                    // not the directory itself. Without it the whole staging
                    // dir lands at `/` under its own temp name and nothing is
                    // actually restored — while the command still succeeds.
                    format!("{}/.", staging.path().display()),
                    format!("{handle}:/"),
                ],
            )
            .await;
            let restarted = unquiesce(bin, handle).await;
            settle(ctx.provider, handle, restored, restarted)
        })
    }
}

/// Stop a container so its files can be replaced underneath it.
///
/// Restoring into a RUNNING service is the silent-corruption case named in
/// #563/#613: the process holds open file handles and its own in-memory state,
/// writes over what was just restored, and the restore appears to succeed. A
/// SQLite database is the usual casualty.
#[cfg(feature = "in-process")]
async fn quiesce(bin: &str, instance: &str) -> Result<(), ServiceError> {
    run(bin, &["stop".to_string(), instance.to_string()]).await
}

/// Start it again.
#[cfg(feature = "in-process")]
async fn unquiesce(bin: &str, instance: &str) -> Result<(), ServiceError> {
    run(bin, &["start".to_string(), instance.to_string()]).await
}

/// Combine a restore outcome with the restart that followed it.
///
/// The restart is attempted WHETHER OR NOT the restore succeeded — leaving a
/// service stopped after a failed restore is its own outage, and a failed
/// restore is exactly when an operator most needs the old version back up.
///
/// Both failures are reported together. Collapsing them loses the distinction
/// that decides what to do next: data restored but service down is a start
/// command away, data not restored and service down is a different emergency.
#[cfg(feature = "in-process")]
fn settle(
    what: &str,
    instance: &str,
    restored: Result<(), ServiceError>,
    restarted: Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    match (restored, restarted) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => Err(ServiceError::Other(format!(
            "{what}: data RESTORED, but `{instance}` did not start again: {e}. \
             The restore landed — start the service."
        ))),
        (Err(e), Ok(())) => Err(ServiceError::Other(format!(
            "{what}: restore FAILED ({e}); `{instance}` was restarted on its \
             previous data."
        ))),
        (Err(e), Err(r)) => Err(ServiceError::Other(format!(
            "{what}: restore FAILED ({e}) AND `{instance}` did not start again \
             ({r}). The service is DOWN and its data is in an unknown state."
        ))),
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

    /// The PBS backup group this unit's snapshots belong in.
    ///
    /// One group per unit — `freyr-radarr`, `baldur-immich-db` — so a single
    /// app can be restored without touching its neighbours. Without
    /// `--backup-id` every snapshot on a host lands in that host's default
    /// group, which is how a dozen containers end up sharing one undivided
    /// history that cannot be pruned or restored per-app (#613 gap 3).
    ///
    /// Sanitized to PBS's id charset (alphanumerics, `-`, `_`, `.`): a
    /// provider or instance carrying a `/` or a space would otherwise be
    /// rejected by the server after the backup has already been read.
    fn backup_id(provider: &str, instance: &str) -> String {
        let raw = if instance.trim().is_empty() {
            provider.to_string()
        } else {
            format!("{provider}-{instance}")
        };
        let cleaned: String = raw
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // Collapse runs introduced by the mapping, and never lead or trail
        // with a separator — `--backup-id -foo-` is refused by the server.
        let mut out = String::with_capacity(cleaned.len());
        for c in cleaned.chars() {
            if c == '-' && out.ends_with('-') {
                continue;
            }
            out.push(c);
        }
        let out = out.trim_matches('-').to_string();
        if out.is_empty() {
            "orca".to_string()
        } else {
            out
        }
    }

    /// Container image carrying `proxmox-backup-client`, for hosts that cannot
    /// install it. Debian-based per the fleet image policy — the client is a
    /// glibc Debian package and that is precisely the constraint being worked
    /// around.
    fn client_image() -> String {
        std::env::var("ORCA_PBS_CLIENT_IMAGE")
            .unwrap_or_else(|_| "debian:bookworm-slim".to_string())
    }

    /// Container runtime to borrow when the client is not installed, or `None`
    /// when there is none to borrow.
    fn container_runtime() -> Option<&'static str> {
        ["docker", "podman"]
            .into_iter()
            .find(|bin| utils::path::which(bin).is_some())
    }

    /// Wrap a `proxmox-backup-client` invocation in a throwaway container.
    ///
    /// `proxmox-backup-client` is a glibc Debian package. **freyr is
    /// Alpine/musl; willow and maple are Slackware.** The PBS binaries link
    /// `libc.so.6`, `ld-linux-x86-64.so.2` and `libapt-pkg.so.7.0`, so there is
    /// no musl build and `gcompat` cannot bridge it (#613 gap 2). Those three
    /// hosts are exactly where the per-container app data lives, so without
    /// this the file-level path is unusable on every host that needs it.
    ///
    /// All three are docker hosts, so the client runs in a container there
    /// instead — an implementation detail of this primitive, not a separate
    /// system.
    ///
    /// Two choices worth stating:
    ///
    /// - Each source is mounted at **the same path inside the container**, so
    ///   the archive specs (`config.pxar:/config`) and every `--exclude` are
    ///   byte-identical whether or not the client is containerized. A path that
    ///   meant one thing installed and another containerized would make the
    ///   excludes silently stop matching.
    /// - Mounts are **read-only**, and the secrets ride as `-e NAME` with no
    ///   value — docker inherits them from this process's environment, so
    ///   `PBS_PASSWORD` never appears in argv where `ps` can read it.
    ///
    /// The runtime binary is NOT a parameter: docker and podman take the same
    /// `run` arguments, so the vector depends only on what is being run.
    fn containerize(image: &str, paths: &[String], client_args: &[String]) -> Vec<String> {
        Self::containerize_with(image, paths, client_args, true)
    }

    /// As [`Self::containerize`], but the mounts are WRITABLE.
    ///
    /// For restore, where the target is the thing being written. Backup mounts
    /// read-only on purpose; reusing that here would fail every write with a
    /// permission error that reads like a PBS fault.
    fn containerize_writable(image: &str, paths: &[String], client_args: &[String]) -> Vec<String> {
        Self::containerize_with(image, paths, client_args, false)
    }

    fn containerize_with(
        image: &str,
        paths: &[String],
        client_args: &[String],
        read_only: bool,
    ) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            // No TTY, no stdin: this is a batch job, and an interactive client
            // waiting on a prompt would hang the backup rather than fail it.
            "-i".to_string(),
        ];
        for p in paths.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            args.push("-v".to_string());
            args.push(if read_only {
                format!("{p}:{p}:ro")
            } else {
                format!("{p}:{p}")
            });
        }
        for key in ["PBS_REPOSITORY", "PBS_PASSWORD", "PBS_FINGERPRINT"] {
            if std::env::var_os(key).is_some() {
                // Name only — the VALUE is inherited, never written to argv.
                args.push("-e".to_string());
                args.push(key.to_string());
            }
        }
        args.push(image.to_string());
        args.push("proxmox-backup-client".to_string());
        args.extend(client_args.iter().cloned());
        args
    }

    /// The snapshot a restore reads, from the artifact a backup wrote.
    ///
    /// `backup` records `pbs:host/<backup-id>`; PBS addresses a snapshot as
    /// `<type>/<id>/<time>`. With no recorded time this resolves to the group's
    /// newest, which is what "restore this unit" means in the absence of a
    /// chosen point.
    ///
    /// Returns `None` for an artifact this method did not write — a `tar`
    /// artifact names a tarball on disk, and feeding that to the PBS client
    /// would produce a confusing client-side error instead of a clear refusal.
    fn snapshot_from(artifact_path: &str, timestamp: &str) -> Option<String> {
        let group = artifact_path.strip_prefix("pbs:")?.trim();
        if group.is_empty() {
            return None;
        }
        // Already fully qualified (three segments): take it as given.
        if group.matches('/').count() >= 2 {
            return Some(group.to_string());
        }
        let t = timestamp.trim();
        if t.is_empty() {
            return Some(format!("{group}/latest"));
        }
        Some(format!("{group}/{t}"))
    }

    /// Restore every declared path from one snapshot.
    ///
    /// Sequential and fail-fast: paths are restored one archive at a time, and
    /// the first failure stops the rest. Carrying on would leave the unit with
    /// some paths at the restored version and some at the current one — a
    /// state that is neither, and that nothing records.
    ///
    /// Routes through the same installed-or-containerized decision the backup
    /// does, so a host that can back up can also restore. The two diverging
    /// would mean a host quietly able to write backups it cannot read.
    async fn restore_paths(snapshot: &str, ctx: &BackupContext<'_>) -> Result<(), ServiceError> {
        for path in ctx.data_paths {
            let args = PbsMethod::restore_args(snapshot, path);
            match utils::path::which("proxmox-backup-client") {
                Some(_) => run("proxmox-backup-client", &args).await?,
                None => {
                    let bin = PbsMethod::container_runtime().ok_or_else(|| {
                        ServiceError::Unsupported(
                            "restore".to_string(),
                            format!(
                                "{}: `proxmox-backup-client` is not installed and no \
                                 container runtime is available to run it in.",
                                ctx.provider
                            ),
                        )
                    })?;
                    let image = PbsMethod::client_image();
                    // The restore TARGET must be writable — the backup path
                    // mounts sources read-only, and reusing that here would
                    // fail every write with a permission error that looks like
                    // a PBS problem.
                    let wrapped =
                        PbsMethod::containerize_writable(&image, std::slice::from_ref(path), &args);
                    run(bin, &wrapped).await?;
                }
            }
        }
        Ok(())
    }

    /// Argument vector for a file-level `proxmox-backup-client restore`.
    ///
    /// One invocation per archive, because `restore` takes exactly one. The
    /// archive name is derived from the path the SAME way `backup_args`
    /// derives it, so a path backed up as `config.pxar` is looked for under
    /// that name and not a second spelling of it.
    fn restore_args(snapshot: &str, path: &str) -> Vec<String> {
        let archive = path.trim_matches('/').replace('/', "_");
        vec![
            "restore".to_string(),
            snapshot.to_string(),
            format!("{archive}.pxar"),
            path.to_string(),
        ]
    }

    /// Argument vector for a file-level `proxmox-backup-client backup`.
    ///
    /// Pure, so the arguments are ASSERTABLE. They were inline before, which
    /// meant the only thing a test could observe was that the binary is absent
    /// in CI — a test that passes identically whether or not the excludes and
    /// the backup id are there at all.
    fn backup_args(
        provider: &str,
        instance: &str,
        paths: &[String],
        exclude: &[String],
    ) -> Vec<String> {
        let mut args = vec!["backup".to_string()];
        for p in paths {
            let archive = p.trim_matches('/').replace('/', "_");
            args.push(format!("{archive}.pxar:{p}"));
        }
        // Excludes BEFORE the group flags, each as its own `--exclude`: the
        // client takes the flag repeatedly, and joining them into one value
        // silently matches nothing.
        for e in exclude.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            args.push("--exclude".to_string());
            args.push(e.to_string());
        }
        // `host` is the type for a filesystem backup that is not a PVE guest.
        args.push("--backup-type".to_string());
        args.push("host".to_string());
        args.push("--backup-id".to_string());
        args.push(Self::backup_id(provider, instance));
        args
    }

    /// Argument vector for a whole-guest `vzdump`.
    ///
    /// Excludes are honored here too. vzdump spells them `--exclude-path`, and
    /// a path the caller wrote for the file-level client is the same path an
    /// operator means for a guest — translating rather than ignoring keeps one
    /// spec meaningful across both branches.
    fn vzdump_args(vmid: &str, storage: &str, exclude: &[String]) -> Vec<String> {
        let mut args = vec![
            vmid.to_string(),
            "--storage".into(),
            storage.to_string(),
            "--mode".into(),
            "snapshot".into(),
        ];
        for e in exclude.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            args.push("--exclude-path".to_string());
            args.push(e.to_string());
        }
        args
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
                        &PbsMethod::vzdump_args(ctx.instance, &storage, ctx.exclude),
                    )
                    .await?;
                    Ok(BackupArtifact {
                        service: ctx.provider.to_string(),
                        instance: ctx.instance.to_string(),
                        path: format!("pbs:{storage}/{}", ctx.instance),
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
                    let args = PbsMethod::backup_args(
                        ctx.provider,
                        ctx.instance,
                        ctx.data_paths,
                        ctx.exclude,
                    );
                    // Installed client wins. Otherwise borrow a container
                    // runtime — on freyr/willow/maple the client CANNOT be
                    // installed, and those are the hosts holding the app data.
                    match utils::path::which("proxmox-backup-client") {
                        Some(_) => run("proxmox-backup-client", &args).await?,
                        None => {
                            // `Unsupported`, not `Other`: this host CANNOT
                            // perform the operation, which is a different fact
                            // from an attempt that was made and failed.
                            let bin = PbsMethod::container_runtime().ok_or_else(|| {
                                ServiceError::Unsupported(
                                    "backup".to_string(),
                                    format!(
                                        "{}: `proxmox-backup-client` is not installed and no \
                                         container runtime is available to run it in. The \
                                         client is a glibc Debian package with no musl build, \
                                         so on Alpine/Slackware hosts a container runtime is \
                                         the only way to run it.",
                                        ctx.provider
                                    ),
                                )
                            })?;
                            let image = PbsMethod::client_image();
                            let wrapped = PbsMethod::containerize(&image, ctx.data_paths, &args);
                            run(bin, &wrapped).await?;
                        }
                    }
                    // The artifact records the GROUP, not the bare instance:
                    // that is the handle a restore has to address, and
                    // reporting something a restore cannot use makes the
                    // artifact unusable for the thing it exists for.
                    Ok(BackupArtifact {
                        service: ctx.provider.to_string(),
                        instance: ctx.instance.to_string(),
                        path: format!(
                            "pbs:host/{}",
                            PbsMethod::backup_id(ctx.provider, ctx.instance)
                        ),
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
        from: &'a BackupArtifact,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            match ctx.runtime {
                // Whole-guest restore (`pct restore` / `qmrestore`) DESTROYS the
                // existing guest and needs a target vmid nobody has stated. It
                // also restores from the whole-guest images #613 gap 1 says
                // should not be taken in the first place, so building it here
                // would deepen the thing that is being removed. Refused, with
                // the command to run by hand.
                Runtime::Lxc | Runtime::Vm => Err(ServiceError::Unsupported(
                    "restore".to_string(),
                    format!(
                        "{}: a whole-guest PBS restore destroys and recreates the guest, \
                         and the target vmid is not implied by anything here. Run it \
                         deliberately: `pct restore <vmid> <volume>` (LXC) or \
                         `qmrestore <volume> <vmid>` (VM). Per-unit file restore is \
                         supported for container runtimes.",
                        ctx.provider
                    ),
                )),
                Runtime::Docker | Runtime::Podman => {
                    if ctx.data_paths.is_empty() {
                        return Err(ServiceError::Other(format!(
                            "{}: no data_paths to restore into",
                            ctx.provider
                        )));
                    }
                    let snapshot = PbsMethod::snapshot_from(&from.path, &from.timestamp)
                        .ok_or_else(|| {
                            ServiceError::Other(format!(
                                "{}: `{}` is not a PBS artifact — this method restores only \
                                 snapshots it wrote (`pbs:<type>/<id>`)",
                                ctx.provider, from.path
                            ))
                        })?;
                    let bin = TarMethod::cli(ctx.runtime).ok_or_else(|| {
                        ServiceError::Unsupported(
                            "restore".to_string(),
                            format!("no container CLI for {:?}", ctx.runtime),
                        )
                    })?;

                    // STOP FIRST. Restoring into a running service lets it
                    // write back over what was just restored from its own
                    // in-memory state: the restore reports success and the data
                    // is a mix of both. A SQLite database is the usual
                    // casualty (#613 gap 5).
                    quiesce(bin, ctx.instance).await?;
                    let restored = PbsMethod::restore_paths(&snapshot, &ctx).await;
                    // Attempted whether or not the restore worked: a failed
                    // restore is exactly when the old version needs to be back
                    // up, and leaving it stopped is its own outage.
                    let restarted = unquiesce(bin, ctx.instance).await;
                    settle(ctx.provider, ctx.instance, restored, restarted)
                }
            }
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
            _instance: &'a str,
            _routes: &'a Routes,
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
            serde_json::json!({"instance":"x","routes":[]}),
        )
        .await
        .expect("status ok");
        assert_eq!(out["healthy"], serde_json::json!(true));

        // workload_spec uses the trait default → unimplemented error value.
        let err = dispatch_op(
            &f,
            "workload_spec",
            serde_json::json!({"runtime":"docker","instance":"x","routes":[]}),
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

    // #615: reachability is the ordered route set and nothing else. The two
    // derived reads the old `Endpoint` provided now live on `Routes`, where they
    // were always pure functions of the routes anyway.
    #[test]
    fn routes_carry_the_address_and_the_two_reads_derived_from_it() {
        let routes = Routes::from(vec![Route::new("lan_v4", "http", "h", Some(4533))]);
        let s = serde_json::to_string(&routes).unwrap();
        let back: Routes = serde_json::from_str(&s).unwrap();

        assert_eq!(back.primary_url(), "http://h:4533");
        assert_eq!(back.publish_port(0), 4533);

        // No route at all is addressable-as-nothing, not a panic, and the
        // caller's own default port stands.
        let none = Routes::new();
        assert_eq!(none.primary_url(), "");
        assert_eq!(none.publish_port(8080), 8080);

        // `publish_port` reads the LOCAL bind specifically: a reach-only route
        // must not be mistaken for the host bind.
        let reach_only = Routes::from(vec![Route::new("fqdn", "https", "x.example", Some(443))]);
        assert_eq!(reach_only.publish_port(8080), 8080);
        assert_eq!(reach_only.primary_url(), "https://x.example:443");
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
            instance: &'a str,
            _routes: &'a Routes,
        ) -> BoxFuture<'a, Result<WorkloadSpec, ServiceError>> {
            let name = format!("{}-{}", instance, runtime_str(runtime));
            Box::pin(async move {
                Ok(WorkloadSpec {
                    name,
                    ..Default::default()
                })
            })
        }
        fn configure<'a>(
            &'a self,
            _instance: &'a str,
            _routes: &'a Routes,
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
            serde_json::json!({"runtime":"docker","instance":"x","routes":[]}),
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
            serde_json::json!({"instance":"x","routes":[],"config":"yaml"}),
        )
        .await
        .expect("configure ok");

        // Backend-returned error is surfaced as an error value.
        let e = dispatch_op(
            &b,
            "configure",
            serde_json::json!({"instance":"x","routes":[],"config":""}),
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
        let e = dispatch_op(&f, "backup", serde_json::json!({"instance":"x"}))
            .await
            .expect_err("backup needs runtime or unimpl");
        // Fake declares runtimes, so the generic backup proceeds to method
        // selection; either way the op errored deterministically (no panic).
        assert!(!e.to_string().is_empty());

        let e = dispatch_op(
            &f,
            "restore",
            serde_json::json!({"instance":"x","from":{"service":"s","instance":"i","path":"/p","timestamp":"t"}}),
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
                let _a: ReachArgs = serde_json::from_str(&args_json).unwrap();
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

        let routes = Routes::from(vec![Route::new("lan_v4", "http", "abs", None)]);
        let st = b.status("main", &routes).await.expect("proxied status");
        assert!(st.healthy);
        assert_eq!(st.detail, "proxied");
        b.configure("main", &routes, "cfg")
            .await
            .expect("proxied configure");

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
    async fn tar_backup_rejects_empty_data_paths_and_bare_vm() {
        let tar = TarMethod;
        let instance = "inst";
        // Empty data_paths → clear guidance error, before any subprocess.
        let e = tar
            .backup(BackupContext {
                runtime: Runtime::Docker,
                instance,
                provider: "abs",
                data_paths: &[],
                exclude: &[],
            })
            .await
            .expect_err("no data_paths");
        assert!(e.to_string().contains("no data_paths"), "{e}");

        // A bare VM has no generic tar CLI → Unsupported, before any subprocess.
        let paths = ["/config".to_string()];
        let e = tar
            .backup(BackupContext {
                runtime: Runtime::Vm,
                instance,
                provider: "abs",
                data_paths: &paths,
                exclude: &[],
            })
            .await
            .expect_err("no tar path on bare vm");
        assert!(matches!(e, ServiceError::Unsupported(_, _)), "{e}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn tar_restore_on_bare_vm_is_unsupported() {
        let tar = TarMethod;
        let instance = "inst";
        let art = BackupArtifact::default();
        let e = tar
            .restore(
                BackupContext {
                    runtime: Runtime::Vm,
                    instance,
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
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
        let method = Some("restic-test");
        assert_eq!(select_method(method, Runtime::Docker).name(), "restic-test");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn select_method_defaults_to_tar_for_containers() {
        // Docker/Podman are never PBS-native, so they always route to tar.
        assert_eq!(select_method(None, Runtime::Docker).name(), "tar");
        assert_eq!(select_method(None, Runtime::Podman).name(), "tar");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn select_method_falls_back_to_tar_when_choice_unregistered() {
        // An explicit but unknown method name doesn't match; falls to the auto
        // choice (tar for a container).
        let method = Some("ghost");
        assert_eq!(select_method(method, Runtime::Docker).name(), "tar");
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
                    name: format!("{}-{}", a.instance, runtime_str(a.runtime)),
                    ..Default::default()
                };
                Ok(serde_json::to_string(&spec).unwrap())
            }
            "backup" => {
                let a: BackupArgs = serde_json::from_str(&args_json).unwrap();
                let art = BackupArtifact {
                    service: "proxied".into(),
                    instance: a.instance.to_string(),
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
        let instance = "main";
        let spec = b
            .workload_spec(Runtime::Docker, instance, &Routes::new())
            .await
            .expect("proxied workload_spec");
        assert_eq!(spec.name, "main-docker");

        let art = b
            .backup(instance, None, None)
            .await
            .expect("proxied backup");
        assert_eq!(art.service, "proxied");
        assert_eq!(art.instance, "main");
        assert_eq!(art.path, "/backups/x.tar.gz");

        let from = BackupArtifact {
            path: "/backups/x.tar.gz".into(),
            ..Default::default()
        };
        b.restore(instance, None, None, &from)
            .await
            .expect("proxied restore");

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
            .status("main", &Routes::new())
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
            .status("main", &Routes::new())
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
    #[serial_test::serial(env)]
    async fn pbs_file_backup_rejects_empty_data_paths() {
        let pbs = PbsMethod;
        let instance = "cont";
        for rt in [Runtime::Docker, Runtime::Podman] {
            let e = pbs
                .backup(BackupContext {
                    runtime: rt,
                    instance,
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
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

    // ── run() / run_program(): the subprocess primitive ──────────────────
    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn run_succeeds_on_zero_exit() {
        run("sh", &["-c".into(), "exit 0".into()])
            .await
            .expect("zero exit is Ok");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn run_maps_nonzero_exit_to_transport_error() {
        let e = run("sh", &["-c".into(), "exit 3".into()])
            .await
            .expect_err("non-zero exit errors");
        assert!(matches!(e, ServiceError::Transport(_)), "{e}");
        assert!(e.to_string().contains("failed"), "{e}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn run_maps_spawn_failure_to_transport_error() {
        let e = run("orca-nonexistent-binary-xyz123", &[])
            .await
            .expect_err("missing binary cannot spawn");
        assert!(matches!(e, ServiceError::Transport(_)), "{e}");
        assert!(e.to_string().contains("spawn"), "{e}");
    }

    // ── TarMethod: command-construction branches (both runtimes) ──────────
    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn tar_backup_builds_and_runs_both_runtime_branches() {
        let tar = TarMethod;
        let instance = "orca-cov-absent-instance";
        let paths = ["/config".to_string()];
        for rt in [Runtime::Docker, Runtime::Lxc] {
            let e = tar
                .backup(BackupContext {
                    runtime: rt,
                    instance,
                    provider: "abs",
                    data_paths: &paths,
                    exclude: &[],
                })
                .await
                .expect_err("no such instance / no runtime binary");
            assert!(matches!(e, ServiceError::Transport(_)), "{rt:?}: {e}");
        }
    }

    /// A fake `docker`/`tar` on a controlled PATH that logs every invocation.
    ///
    /// Returns `(dir, logfile)`. `tar` succeeds (so staging works); `docker`
    /// succeeds for stop/start and fails otherwise, which exercises the
    /// restart-anyway path without needing a container runtime.
    #[cfg(feature = "in-process")]
    fn fake_cli(copy_succeeds: bool) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let cp_exit = if copy_succeeds { 0 } else { 9 };
        for (name, body) in [
            (
                "docker",
                format!(
                    "#!/bin/sh\necho \"docker $@\" >> {}\n\
                     case \"$1\" in stop|start) exit 0 ;; *) exit {cp_exit} ;; esac\n",
                    log.display()
                ),
            ),
            (
                "tar",
                format!("#!/bin/sh\necho \"tar $@\" >> {}\nexit 0\n", log.display()),
            ),
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, body).expect("write fake");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        (dir, log)
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn a_container_tar_restore_stops_the_unit_before_writing_to_it() {
        // #676: the old path `cp`-ed the tarball in and `exec`-ed tar INSIDE a
        // running unit, so the service wrote back over what had just been
        // restored from its own in-memory state — and reported success.
        //
        // The ORDER is the correctness, so the order is what is asserted.
        let (dir, log) = fake_cli(true);
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);

        TarMethod
            .restore(
                BackupContext {
                    runtime: Runtime::Docker,
                    instance: "sonarr",
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
                },
                &BackupArtifact {
                    path: "/var/tmp/orca-backups/abs-sonarr-1.tar.gz".into(),
                    ..Default::default()
                },
            )
            .await
            .expect("fake docker succeeds");

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = calls.lines().collect();
        let at = |needle: &str| lines.iter().position(|l| l.starts_with(needle));

        let stage = at("tar xzf").expect("staged host-side");
        let stop = at("docker stop sonarr").expect("stopped");
        let copy = at("docker cp").expect("copied in");
        let start = at("docker start sonarr").expect("restarted");

        assert!(stop < copy, "must STOP before writing: {lines:?}");
        assert!(copy < start, "must restart AFTER writing: {lines:?}");
        assert!(stage < copy, "staging precedes the copy: {lines:?}");
        // `exec` cannot run in a stopped container, and relying on it is what
        // forced the old path to leave the unit up.
        assert!(
            !lines.iter().any(|l| l.starts_with("docker exec")),
            "a quiesced restore must not exec inside the unit: {lines:?}"
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn the_copy_takes_the_staging_directorys_contents_not_the_directory() {
        // `docker cp <dir> c:/` puts the staging dir itself at `/` under its
        // temp name and SUCCEEDS, restoring nothing. The trailing `/.` is what
        // copies the contents, and nothing else would catch its loss.
        let (dir, log) = fake_cli(true);
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);
        TarMethod
            .restore(
                BackupContext {
                    runtime: Runtime::Docker,
                    instance: "sonarr",
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
                },
                &BackupArtifact {
                    path: "/var/tmp/x.tar.gz".into(),
                    ..Default::default()
                },
            )
            .await
            .expect("ok");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let cp = calls
            .lines()
            .find(|l| l.starts_with("docker cp"))
            .expect("cp present")
            .to_string();
        assert!(cp.contains("/. "), "must copy CONTENTS: {cp}");
        assert!(cp.trim_end().ends_with("sonarr:/"), "{cp}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn a_failed_container_restore_still_restarts_the_unit() {
        let (dir, log) = fake_cli(false);
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);
        let e = TarMethod
            .restore(
                BackupContext {
                    runtime: Runtime::Docker,
                    instance: "sonarr",
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
                },
                &BackupArtifact {
                    path: "/var/tmp/x.tar.gz".into(),
                    ..Default::default()
                },
            )
            .await
            .expect_err("copy fails");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.lines().any(|l| l.starts_with("docker start sonarr")),
            "a failed restore is exactly when the old version must come back \
             up: {calls}"
        );
        let m = e.to_string();
        assert!(m.contains("restore FAILED"), "{m}");
        assert!(m.contains("previous data"), "{m}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn an_lxc_restore_stays_on_its_live_transport() {
        // `pct push`/`pct exec` both require the container UP, so there is no
        // quiesced form of this transport. It must NOT be stopped — doing so
        // breaks the very commands the path depends on.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let pct = dir.path().join("pct");
        std::fs::write(
            &pct,
            format!("#!/bin/sh\necho \"pct $@\" >> {}\nexit 0\n", log.display()),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pct, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);

        TarMethod
            .restore(
                BackupContext {
                    runtime: Runtime::Lxc,
                    instance: "113",
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
                },
                &BackupArtifact {
                    path: "/var/tmp/x.tar.gz".into(),
                    ..Default::default()
                },
            )
            .await
            .expect("ok");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(calls.contains("pct push"), "{calls}");
        assert!(calls.contains("pct exec"), "{calls}");
        assert!(
            !calls.contains("pct stop"),
            "stopping an LXC breaks push/exec, the transport this path needs: {calls}"
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn tar_excludes_precede_the_paths_they_apply_to() {
        let cmd = TarMethod::tar_cmd(
            &["/config".to_string(), "/data".to_string()],
            &["/config/cache".to_string()],
        );
        let ex = cmd.find("--exclude=").expect("exclude present");
        let first_path = cmd.find("'/config'").expect("path present");
        // GNU tar applies --exclude to operands that FOLLOW it. Trailing
        // excludes parse without complaint and match nothing.
        assert!(ex < first_path, "{cmd}");
        assert!(cmd.contains("--exclude='/config/cache'"), "{cmd}");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn tar_quotes_paths_so_a_space_cannot_split_one_in_two() {
        let cmd = TarMethod::tar_cmd(&["/mnt/My Data".to_string()], &[]);
        assert!(cmd.contains("'/mnt/My Data'"), "{cmd}");
        // An embedded quote must not end the quoting and let the rest of the
        // path be read as shell.
        let nasty = TarMethod::tar_cmd(&["/a'b".to_string()], &[]);
        assert!(nasty.contains(r#"'/a'\''b'"#), "{nasty}");
    }

    // ── #613: the arguments themselves, not just "the binary is absent" ───
    //
    // These assert the argv. The pre-existing branch tests below can only
    // observe that `vzdump`/`proxmox-backup-client` are missing in CI, which
    // they do whether or not the excludes and the backup id are passed at
    // all — they passed throughout the period the excludes were dropped.

    // ── #613 gap 5: restore ──────────────────────────────────────────────

    #[cfg(feature = "in-process")]
    #[test]
    fn a_restore_reads_back_exactly_what_the_backup_wrote() {
        // The archive name must be derived the SAME way on both sides. A
        // second spelling means a restore that cannot find its own backup.
        let backup = PbsMethod::backup_args("abs", "freyr", &["/config".to_string()], &[]);
        let archive = backup
            .iter()
            .find(|a| a.ends_with(".pxar:/config"))
            .expect("archive spec present")
            .split(':')
            .next()
            .expect("archive name")
            .to_string();
        let restore = PbsMethod::restore_args("host/abs-freyr/latest", "/config");
        assert_eq!(restore[2], archive, "backup wrote `{archive}`");
        assert_eq!(restore[0], "restore");
        assert_eq!(restore[1], "host/abs-freyr/latest");
        // Restored back to the path it came from.
        assert_eq!(restore[3], "/config");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn a_snapshot_resolves_from_the_artifact_a_backup_recorded() {
        // What `backup` actually writes into the artifact.
        assert_eq!(
            PbsMethod::snapshot_from("pbs:host/abs-freyr", "1759300000").as_deref(),
            Some("host/abs-freyr/1759300000")
        );
        // No recorded time: the group's newest, which is what "restore this
        // unit" means with no chosen point.
        assert_eq!(
            PbsMethod::snapshot_from("pbs:host/abs-freyr", "").as_deref(),
            Some("host/abs-freyr/latest")
        );
        // Already fully qualified — taken as given, not re-suffixed.
        assert_eq!(
            PbsMethod::snapshot_from("pbs:host/abs-freyr/2026-10-01T00:00:00Z", "99").as_deref(),
            Some("host/abs-freyr/2026-10-01T00:00:00Z")
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn a_foreign_artifact_is_refused_not_guessed_at() {
        // A `tar` artifact names a tarball on disk. Handing that to the PBS
        // client yields a confusing client-side error instead of a clear
        // refusal, and there is no sense in which it could succeed.
        assert_eq!(
            PbsMethod::snapshot_from("/var/tmp/orca-backups/abs-cont-123.tar.gz", "1"),
            None
        );
        assert_eq!(PbsMethod::snapshot_from("pbs:", "1"), None);
        assert_eq!(PbsMethod::snapshot_from("", "1"), None);
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn a_restore_target_is_mounted_writable_unlike_a_backup_source() {
        // Backup mounts read-only on purpose. Reusing that for restore fails
        // every write with a permission error that reads like a PBS fault.
        let w = PbsMethod::containerize_writable(
            "img",
            &["/config".to_string()],
            &["restore".to_string()],
        );
        assert!(
            w.windows(2)
                .any(|p| p[0] == "-v" && p[1] == "/config:/config"),
            "{w:?}"
        );
        assert!(!w.iter().any(|a| a.ends_with(":ro")), "{w:?}");
        // And the backup path is unchanged — still read-only.
        let r = PbsMethod::containerize("img", &["/config".to_string()], &["backup".to_string()]);
        assert!(
            r.windows(2)
                .any(|p| p[0] == "-v" && p[1] == "/config:/config:ro"),
            "{r:?}"
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn a_failed_restore_and_a_failed_restart_are_reported_separately() {
        let t = || ServiceError::Transport("boom".into());
        let r = || ServiceError::Transport("no start".into());

        // Restored, service down: one command from fine. Say so.
        let e = settle("abs", "cont", Ok(()), Err(r())).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("RESTORED"), "{m}");
        assert!(m.contains("start the service"), "{m}");

        // Restore failed but the service came back on its old data: bad, not
        // an emergency.
        let m = settle("abs", "cont", Err(t()), Ok(()))
            .unwrap_err()
            .to_string();
        assert!(m.contains("restore FAILED"), "{m}");
        assert!(m.contains("previous data"), "{m}");

        // Both failed: the distinction that decides what to do next.
        let m = settle("abs", "cont", Err(t()), Err(r()))
            .unwrap_err()
            .to_string();
        assert!(m.contains("DOWN"), "{m}");
        assert!(m.contains("unknown state"), "{m}");

        assert!(settle("abs", "cont", Ok(()), Ok(())).is_ok());
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn a_whole_guest_restore_is_refused_with_the_command_to_run() {
        // `pct restore`/`qmrestore` destroy and recreate the guest, and the
        // target vmid is not implied by anything here. Guessing it is the one
        // unrecoverable mistake this verb could make.
        for rt in [Runtime::Lxc, Runtime::Vm] {
            let e = PbsMethod
                .restore(
                    BackupContext {
                        runtime: rt,
                        instance: "113",
                        provider: "abs",
                        data_paths: &[],
                        exclude: &[],
                    },
                    &BackupArtifact::default(),
                )
                .await
                .expect_err("must refuse");
            assert!(matches!(e, ServiceError::Unsupported(..)), "{rt:?}: {e}");
            let m = e.to_string();
            assert!(m.contains("pct restore"), "{rt:?}: {m}");
            assert!(m.contains("qmrestore"), "{rt:?}: {m}");
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn a_restore_stops_the_unit_before_touching_its_files() {
        // The ordering IS the correctness here: restoring into a running
        // service lets it write back over what was just restored from its own
        // in-memory state, and the restore reports success either way.
        //
        // A fake container CLI logs every invocation, so the sequence is
        // observable. It exits 0 for stop/start and non-zero otherwise, so the
        // restore step fails — which also exercises the restart-anyway path.
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let fake = dir.path().join("docker");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\ncase \"$1\" in stop|start) exit 0 ;; *) exit 9 ;; esac\n",
                log.display()
            ),
        )
        .expect("write fake docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);

        let paths = ["/config".to_string()];
        let e = PbsMethod
            .restore(
                BackupContext {
                    runtime: Runtime::Docker,
                    instance: "sonarr",
                    provider: "abs",
                    data_paths: &paths,
                    exclude: &[],
                },
                &BackupArtifact {
                    path: "pbs:host/abs-sonarr".into(),
                    timestamp: "1759300000".into(),
                    ..Default::default()
                },
            )
            .await
            .expect_err("fake client exits 9");

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = calls.lines().collect();
        assert!(
            lines.first().is_some_and(|l| l.starts_with("stop sonarr")),
            "the unit must be STOPPED first: {lines:?}"
        );
        assert!(
            lines.last().is_some_and(|l| l.starts_with("start sonarr")),
            "the unit must be restarted even though the restore failed: {lines:?}"
        );
        // And the failure says the data was not restored, so nobody reads a
        // restarted service as a successful restore.
        let m = e.to_string();
        assert!(m.contains("restore FAILED"), "{m}");
    }

    // ── #613 gap 2: the client cannot exist on the hosts that need it ────

    /// Set env vars for the duration of a test and restore them after, even on
    /// panic. The PBS variables are read from the process environment by
    /// design (that is how the secret stays out of argv), so testing that
    /// behaviour means touching the real environment.
    #[cfg(feature = "in-process")]
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    #[cfg(feature = "in-process")]
    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let prior = vars
                .iter()
                .map(|(k, _)| (*k, std::env::var_os(k)))
                .collect();
            for (k, v) in vars {
                // SAFETY: every test that mutates the environment OR spawns a
                // subprocess is in the `serial(env)` group, so no other test
                // thread is reading or writing the environment concurrently.
                // Both halves matter: `std::env` is process-global, so a test
                // left out of the group sees a PATH holding only `fake_cli`'s
                // stubs and fails on spawn (or silently runs the wrong binary).
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
            Self(prior)
        }
    }

    #[cfg(feature = "in-process")]
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                // SAFETY: as above.
                unsafe {
                    match v {
                        Some(v) => std::env::set_var(k, v),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn the_containerized_client_runs_the_identical_client_argv() {
        // The point of the wrapper is that it changes WHERE the client runs
        // and nothing about WHAT it runs. If the two argvs ever diverge, the
        // excludes and the backup id silently apply on one host and not
        // another, which is worse than the client being missing.
        let client = PbsMethod::backup_args(
            "radarr",
            "freyr",
            &["/config".to_string()],
            &["/config/MediaCover".to_string()],
        );
        let wrapped = PbsMethod::containerize("img", &["/config".to_string()], &client);
        let at = wrapped
            .iter()
            .position(|a| a == "proxmox-backup-client")
            .expect("client invoked");
        assert_eq!(&wrapped[at + 1..], &client[..]);
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn sources_mount_at_the_same_path_read_only() {
        let wrapped = PbsMethod::containerize(
            "img",
            &["/config".to_string(), "/opt/halvor".to_string()],
            &["backup".to_string()],
        );
        let mounts: Vec<&String> = wrapped
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped[i - 1] == "-v")
            .map(|(_, v)| v)
            .collect();
        // Same path inside as out: an archive spec of `config.pxar:/config`
        // has to resolve to the same bytes either way.
        assert_eq!(
            mounts,
            vec!["/config:/config:ro", "/opt/halvor:/opt/halvor:ro"]
        );
        // A backup has no business being able to write to its sources.
        assert!(mounts.iter().all(|m| m.ends_with(":ro")), "{mounts:?}");
    }

    #[cfg(feature = "in-process")]
    #[test]
    #[serial_test::serial(env)]
    fn the_pbs_password_never_appears_in_argv() {
        // argv is world-readable through `ps`. The secret must be INHERITED by
        // name, never written as `-e NAME=value`.
        let _g = EnvGuard::set(&[
            ("PBS_REPOSITORY", Some("pbs@host:store")),
            ("PBS_PASSWORD", Some("hunter2-should-never-appear")),
        ]);
        let wrapped = PbsMethod::containerize("img", &[], &["backup".to_string()]);
        assert!(
            !wrapped.iter().any(|a| a.contains("hunter2")),
            "secret leaked into argv: {wrapped:?}"
        );
        // Passed by name so the value is inherited from this process.
        assert!(
            wrapped
                .windows(2)
                .any(|w| w[0] == "-e" && w[1] == "PBS_PASSWORD"),
            "{wrapped:?}"
        );
        assert!(
            wrapped
                .windows(2)
                .any(|w| w[0] == "-e" && w[1] == "PBS_REPOSITORY"),
            "{wrapped:?}"
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    #[serial_test::serial(env)]
    fn an_absent_pbs_variable_is_not_forwarded_as_an_empty_one() {
        // `-e PBS_FINGERPRINT` with nothing behind it hands the client an
        // empty value, which is not the same as not setting it.
        let _g = EnvGuard::set(&[
            ("PBS_REPOSITORY", Some("pbs@host:store")),
            ("PBS_PASSWORD", None),
            ("PBS_FINGERPRINT", None),
        ]);
        let wrapped = PbsMethod::containerize("img", &[], &["backup".to_string()]);
        assert!(
            !wrapped.iter().any(|a| a == "PBS_FINGERPRINT"),
            "{wrapped:?}"
        );
        assert!(!wrapped.iter().any(|a| a == "PBS_PASSWORD"), "{wrapped:?}");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn the_container_is_throwaway_and_non_interactive() {
        let wrapped = PbsMethod::containerize("img", &[], &["backup".to_string()]);
        assert_eq!(wrapped[0], "run");
        // Without --rm a nightly backup leaves 365 dead containers a year.
        assert!(wrapped.contains(&"--rm".to_string()), "{wrapped:?}");
        // No TTY: a client that stops to prompt must fail the backup, not hang it.
        assert!(wrapped.contains(&"-i".to_string()), "{wrapped:?}");
        assert!(!wrapped.contains(&"-t".to_string()), "{wrapped:?}");
    }

    #[cfg(feature = "in-process")]
    #[test]
    #[serial_test::serial(env)]
    fn the_image_is_overridable_and_defaults_to_debian() {
        // The client is a glibc Debian package — that constraint is the whole
        // reason this path exists, so the default base cannot be Alpine.
        {
            let _g = EnvGuard::set(&[("ORCA_PBS_CLIENT_IMAGE", None)]);
            assert!(PbsMethod::client_image().starts_with("debian:"));
        }
        let _g = EnvGuard::set(&[("ORCA_PBS_CLIENT_IMAGE", Some("my/pbs:1"))]);
        assert_eq!(PbsMethod::client_image(), "my/pbs:1");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn pbs_file_backup_passes_every_exclude_as_its_own_flag() {
        let args = PbsMethod::backup_args(
            "radarr",
            "freyr",
            &["/config".to_string()],
            &[
                "/config/MediaCover".to_string(),
                "/config/Backups".to_string(),
                "/config/logs".to_string(),
            ],
        );
        // One `--exclude` per path. Joining them into a single value matches
        // nothing and silently backs up the lot — the 1.9G-vs-145M case.
        let flags: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, _)| i > &0 && args[i - 1] == "--exclude")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            flags,
            vec!["/config/MediaCover", "/config/Backups", "/config/logs"]
        );
        assert_eq!(args.iter().filter(|a| *a == "--exclude").count(), 3);
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn pbs_file_backup_lands_in_a_per_unit_group() {
        let args = PbsMethod::backup_args("radarr", "freyr", &["/config".to_string()], &[]);
        let at = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args[i + 1].clone())
        };
        // Without these every container on a host shares one undivided
        // history that cannot be pruned or restored per-app.
        assert_eq!(at("--backup-type").as_deref(), Some("host"));
        assert_eq!(at("--backup-id").as_deref(), Some("radarr-freyr"));
        // The archive spec is still first and still correct.
        assert_eq!(args[0], "backup");
        assert_eq!(args[1], "config.pxar:/config");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn a_backup_id_is_always_something_pbs_will_accept() {
        // A provider or instance with a slash or a space would be rejected by
        // the server AFTER the data has been read — an expensive way to fail.
        assert_eq!(
            PbsMethod::backup_id("immich/db", "baldur stack"),
            "immich-db-baldur-stack"
        );
        // No leading, trailing, or doubled separators.
        assert_eq!(PbsMethod::backup_id("/weird/", "//x//"), "weird-x");
        // An empty instance is the provider alone, not a dangling separator.
        assert_eq!(PbsMethod::backup_id("radarr", ""), "radarr");
        assert_eq!(PbsMethod::backup_id("radarr", "   "), "radarr");
        // Never empty: an empty `--backup-id` is refused.
        assert_eq!(PbsMethod::backup_id("///", ""), "orca");
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn vzdump_honors_the_same_excludes_under_its_own_spelling() {
        let args = PbsMethod::vzdump_args("102", "pbs", &["/var/lib/docker".to_string()]);
        assert_eq!(args[0], "102");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--storage" && w[1] == "pbs")
        );
        // vzdump spells it `--exclude-path`; one spec must mean the same thing
        // on both branches or an operator's exclusions apply only sometimes.
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--exclude-path" && w[1] == "/var/lib/docker"),
            "{args:?}"
        );
    }

    #[cfg(feature = "in-process")]
    #[test]
    fn empty_and_blank_excludes_are_dropped_not_passed_through() {
        // A blank `--exclude ""` is not a no-op to the client; it is an
        // argument it has to interpret.
        let args = PbsMethod::backup_args(
            "p",
            "i",
            &["/data".to_string()],
            &[String::new(), "  ".to_string(), " /data/cache ".to_string()],
        );
        assert_eq!(args.iter().filter(|a| *a == "--exclude").count(), 1);
        assert!(args.contains(&"/data/cache".to_string()), "{args:?}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn a_backends_declared_excludes_reach_the_method() {
        // The regression this closes: the generic path built its context from
        // `data_paths()`, so a backend could declare excludes in its spec and
        // watch them vanish one layer above every method.
        use contract::backup::{BackupSpec, BackupStrategy};
        struct Picky;
        impl ServiceBackend for Picky {
            fn provider(&self) -> &str {
                "picky"
            }
            fn runtimes(&self) -> Vec<Runtime> {
                vec![Runtime::Docker]
            }
            fn default_port(&self) -> u16 {
                0
            }
            fn backup_spec(&self) -> BackupSpec {
                BackupSpec {
                    include: vec!["/config".to_string()],
                    exclude: vec!["/config/cache".to_string()],
                    strategies: vec![BackupStrategy::Paths],
                }
            }
        }

        /// Captures the context it is handed instead of running anything.
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl BackupMethod for Capture {
            fn name(&self) -> &str {
                "capture"
            }
            fn backup<'a>(
                &'a self,
                ctx: BackupContext<'a>,
            ) -> BoxFuture<'a, Result<BackupArtifact, ServiceError>> {
                let seen = std::sync::Arc::clone(&self.0);
                Box::pin(async move {
                    *seen.lock().expect("capture poisoned") = ctx.exclude.to_vec();
                    Ok(BackupArtifact {
                        service: ctx.provider.to_string(),
                        instance: ctx.instance.to_string(),
                        path: "captured".into(),
                        ..Default::default()
                    })
                })
            }
            fn restore<'a>(
                &'a self,
                _ctx: BackupContext<'a>,
                _from: &'a BackupArtifact,
            ) -> BoxFuture<'a, Result<(), ServiceError>> {
                Box::pin(async move { Ok(()) })
            }
        }

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        register_method(std::sync::Arc::new(Capture(std::sync::Arc::clone(&seen))));
        Picky
            .backup("inst", Some(Runtime::Docker), Some("capture"))
            .await
            .expect("captured");
        assert_eq!(
            *seen.lock().expect("capture poisoned"),
            vec!["/config/cache".to_string()],
            "the backend's declared excludes must reach the method"
        );
    }

    // ── PbsMethod: whole-guest + file-backup command branches ─────────────
    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn pbs_backup_guest_branch_runs_vzdump() {
        let pbs = PbsMethod;
        let instance = "100";
        for rt in [Runtime::Lxc, Runtime::Vm] {
            let e = pbs
                .backup(BackupContext {
                    runtime: rt,
                    instance,
                    provider: "abs",
                    data_paths: &[],
                    exclude: &[],
                })
                .await
                .expect_err("vzdump absent in CI");
            assert!(matches!(e, ServiceError::Transport(_)), "{rt:?}: {e}");
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn pbs_file_backup_branch_runs_backup_client() {
        // PATH is emptied so the outcome does not depend on what happens to be
        // installed on the machine running the suite. Previously this asserted
        // `Transport` and passed for DIFFERENT reasons in different places: on
        // a dev box with docker it exercised the containerized path, in CI it
        // exercised the missing-binary path. That divergence is what let a
        // behaviour change land green locally and red in CI.
        let empty = tempfile::tempdir().expect("tempdir");
        let _g = EnvGuard::set(&[("PATH", empty.path().to_str())]);

        let pbs = PbsMethod;
        let paths = ["/config".to_string(), "/data".to_string()];
        let e = pbs
            .backup(BackupContext {
                runtime: Runtime::Docker,
                instance: "cont",
                provider: "abs",
                data_paths: &paths,
                exclude: &[],
            })
            .await
            .expect_err("neither client nor container runtime on PATH");
        // Cannot be done here at all — not an attempt that failed.
        assert!(matches!(e, ServiceError::Unsupported(..)), "{e}");
        // And it must say WHY, since "not supported" alone sends an operator
        // looking at the wrong thing.
        let msg = e.to_string();
        assert!(msg.contains("proxmox-backup-client"), "{msg}");
        assert!(msg.contains("musl"), "{msg}");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn the_container_runtime_is_used_when_the_client_is_absent() {
        // A PATH holding a container runtime and NO client: the exact shape of
        // freyr/willow/maple. The fake `docker` exits non-zero, so reaching it
        // surfaces as a Transport error — which is the evidence that the
        // containerized path was taken rather than refused.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("docker");
        std::fs::write(&fake, "#!/bin/sh\nexit 7\n").expect("write fake docker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake docker");
        }
        let _g = EnvGuard::set(&[("PATH", dir.path().to_str())]);

        let paths = ["/config".to_string()];
        let e = PbsMethod
            .backup(BackupContext {
                runtime: Runtime::Docker,
                instance: "cont",
                provider: "abs",
                data_paths: &paths,
                exclude: &[],
            })
            .await
            .expect_err("fake docker exits 7");
        assert!(
            matches!(e, ServiceError::Transport(_)),
            "a reachable container runtime must be ATTEMPTED, not refused: {e}"
        );
    }

    // ── Trait-default lifecycle ops on a minimal backend ─────────────────
    // A backend that overrides nothing beyond the three required methods:
    // its generic backup/restore hit the "no runtime" guard, and
    // configure/status/workload_spec fall through to the unimplemented default.
    struct Minimal;
    impl ServiceBackend for Minimal {
        fn provider(&self) -> &str {
            "minimal"
        }
        fn runtimes(&self) -> Vec<Runtime> {
            Vec::new()
        }
        fn default_port(&self) -> u16 {
            0
        }
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn generic_backup_errors_without_a_runtime() {
        // No declared runtimes and no endpoint.runtime → the generic backup
        // has nothing to back up against.
        let e = Minimal
            .backup("main", None, None)
            .await
            .expect_err("no runtime to select");
        assert!(
            e.to_string().contains("no runtime to back up against"),
            "{e}"
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn generic_restore_errors_without_a_runtime() {
        let e = Minimal
            .restore("main", None, None, &BackupArtifact::default())
            .await
            .expect_err("no runtime to select");
        assert!(
            e.to_string().contains("no runtime to restore against"),
            "{e}"
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn trait_default_configure_status_workload_spec_are_unimplemented() {
        let routes = Routes::new();
        let e = Minimal
            .configure("main", &routes, "cfg")
            .await
            .expect_err("default");
        assert_eq!(e.to_string(), "`configure` not yet implemented");
        let e = Minimal.status("main", &routes).await.expect_err("default");
        assert_eq!(e.to_string(), "`status` not yet implemented");
        let e = Minimal
            .workload_spec(Runtime::Docker, "main", &routes)
            .await
            .expect_err("default");
        assert_eq!(e.to_string(), "`workload_spec` not yet implemented");
    }

    // The endpoint.runtime overrides the backend's (empty) runtime list, so the
    // generic backup proceeds to method selection even for a Minimal backend.
    #[cfg(feature = "in-process")]
    #[tokio::test]
    #[serial_test::serial(env)]
    async fn generic_backup_uses_the_runtime_override_it_is_given() {
        // Minimal declares no data_paths → tar method rejects with a clear error
        // before any subprocess.
        let e = Minimal
            .backup("orca-cov-absent", Some(Runtime::Docker), None)
            .await
            .expect_err("no data_paths");
        assert!(e.to_string().contains("no data_paths"), "{e}");
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
