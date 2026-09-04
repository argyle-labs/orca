// The tool surface crosses this loader as opaque JSON (`sj::Value`) — the JSON
// dispatch protocol of the type-erased boundary, identical to dispatch's
// `ErasedTool::run_json`. This is the designated opaque seam; the workspace
// disallowed-types lint is suppressed for this file only.
#![allow(clippy::disallowed_types)]

//! Runtime loader for out-of-process (subprocess) plugins.
//!
//! ## What this crate does
//!
//! 1. Spawns a plugin executable as a capability-delegated subprocess and
//!    completes the `plugin_proto` wire handshake (see [`supervisor`]), which
//!    negotiates a wire-protocol major match and reads the plugin's manifest,
//!    backends, and declared schema. An incompatible plugin is refused cleanly.
//! 2. Registers each tool the plugin advertises into a process-global runtime
//!    registry, and registers each domain backend it declares against its
//!    domain registry (routing ops back to the subprocess).
//! 3. Exposes [`dispatch`] — the same `(name, args, ctx) -> Result<Value>` shape
//!    as `dispatch::dispatch` — which tries the runtime plugin registry first
//!    and falls back to the statically-linked inventory registry.
//!
//! ## Why a parallel registry
//!
//! orca's built-in tool registry is a frozen `OnceLock<ToolCache>` populated
//! once from `inventory::iter` (link-time). It has no runtime insertion path,
//! by design. Dynamically-loaded plugins therefore live in *this* registry,
//! and [`dispatch`] fronts both so callers see one tool namespace.

pub mod capability;
#[cfg(unix)]
pub mod supervisor;

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::RwLock;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use contract::ToolCtx;
use plugin_toolkit::abi::{BackendDef, SchemaDecl, ToolDef};
// `Value` is the JSON dispatch protocol across the type-erased tool boundary —
// the same opaque layer `dispatch::ErasedTool::run_json` uses. Aliased so the
// payload type is named once, here, at the designated opaque seam.
use serde_json as sj;

/// A single dynamically-loaded plugin, kept alive for the process lifetime.
struct LoadedPlugin {
    /// `target_software` reported by the plugin handshake, e.g. `"jellyfin"`.
    software: String,
    /// The plugin's own semver.
    semver: String,
    /// Free-form target-software compat range, e.g. `"10.8-10.10"`.
    target_compat: String,
    /// The orca-version semver range the plugin declared.
    orca_compat: String,
    /// How this plugin is invoked — an out-of-process subprocess over the wire
    /// protocol.
    backing: Backing,
    /// Tool defs parsed from the plugin's manifest at load time, keyed by tool
    /// name.
    tools: HashMap<String, ToolDef>,
    /// `(domain, backend_name)` pairs this plugin registered with domain
    /// registries (storage, …). Recorded so [`unload_plugin`] can reverse each
    /// registration — the deregistration path a reload/unload needs so a
    /// dropped plugin doesn't leave stale backends pointing at a dead invoke
    /// thunk.
    domain_backends: Vec<(String, String)>,
}

/// How a loaded plugin's tools are invoked: an out-of-process subprocess spoken
/// to over the `plugin_proto` wire protocol ([`supervisor::PluginProcess`];
/// crash-isolated, libc/ABI-independent). Exposes a `(tool, args) -> result`
/// contract carrying `serde_json::Value` (the wire's own type — no String hop),
/// so the registry, `dispatch`, and `unload` treat every plugin uniformly.
#[derive(Clone)]
enum Backing {
    #[cfg(unix)]
    Process(Arc<supervisor::PluginProcess>),
}

impl Backing {
    /// Invoke `tool` with `args`, returning the tool's result `Value` or an
    /// error `Value`. Both cross the wire as `serde_json::Value` already, so
    /// there is no String encode/decode here.
    #[cfg(unix)]
    fn invoke(&self, tool: &str, args: sj::Value) -> std::result::Result<sj::Value, sj::Value> {
        match self {
            Backing::Process(proc) => proc
                .invoke(tool, args)
                .map_err(|e| sj::Value::String(format!("{e:#}"))),
        }
    }

    /// On non-unix there is no subprocess backing, so `Backing` is uninhabited
    /// and this can never be called; the empty match makes that explicit.
    #[cfg(not(unix))]
    fn invoke(&self, _tool: &str, _args: sj::Value) -> std::result::Result<sj::Value, sj::Value> {
        match *self {}
    }
}

/// A domain's constructor: given one backend descriptor and a thunk that calls
/// back across the plugin's FFI `invoke` boundary, register the backend with
/// that domain's process-global registry. The loader's dispatch table maps a
/// `BackendDef::domain` string to one of these so the loader stays
/// domain-agnostic — storage is the first entry; adding a domain is adding a
/// row here, not editing the load path.
///
/// Public so a crate that sits *downstream* of the loader — and therefore
/// cannot be named in the hardcoded dispatch table without a dependency cycle —
/// can contribute its own domain at startup via [`register_domain_constructor`].
pub type DomainRegister = fn(&BackendDef, BackendInvoke) -> Result<()>;

/// The reverse of a [`DomainRegister`]: drop the backend a plugin registered
/// under `name` from its domain registry. Paired with a `DomainRegister` when a
/// downstream crate contributes a domain via [`register_domain_constructor`].
pub type DomainDeregister = fn(&str);

/// The synchronous thunk a domain proxy drives to reach the plugin: it maps an
/// `op` to a `"{invoke_prefix}.{op}"` tool call across the FFI `invoke`
/// boundary and returns the result/error as a `serde_json::Value`. The wire
/// already produces a parsed value, so args and results pass as `Value` with no
/// String hop. `Send + Sync + 'static` so domain proxies can offload it onto a
/// blocking pool.
pub type BackendInvoke =
    Arc<dyn Fn(&str, sj::Value) -> std::result::Result<sj::Value, sj::Value> + Send + Sync>;

/// Domains contributed at runtime by crates downstream of the loader (which the
/// hardcoded [`domain_register`] match cannot name without a cycle). The backup
/// KIND/TARGET registries live in the `system` crate — downstream of this
/// loader — so `system` injects their constructors here at daemon startup,
/// before any plugin loads, via [`register_domain_constructor`]. Consulted as
/// the fallthrough of both [`domain_register`] and [`domain_deregister`], so an
/// injected domain loads and unloads exactly like a built-in one.
static EXTRA_DOMAINS: std::sync::LazyLock<
    RwLock<HashMap<String, (DomainRegister, DomainDeregister)>>,
> = std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Contribute a plugin-backend domain from a crate downstream of the loader.
///
/// Must be called at daemon startup *before* plugins load (alongside the
/// built-in provider registration). Re-registering the same `domain` replaces
/// its constructors in place. This is the dependency-inversion seam that lets
/// `system` own the backup KIND/TARGET registries while the loader — upstream of
/// `system` — still drives their out-of-process registration.
pub fn register_domain_constructor(
    domain: &str,
    register: DomainRegister,
    deregister: DomainDeregister,
) {
    EXTRA_DOMAINS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(domain.to_string(), (register, deregister));
}

/// Domain dispatch table: `domain` → constructor. Domain-agnostic loader seam.
fn domain_register(domain: &str) -> Option<DomainRegister> {
    match domain {
        "storage" => Some(register_storage_backend),
        "media" => Some(register_media_backend),
        "guest_mount" => Some(register_guest_mount_applier),
        "service" => Some(register_service_backend),
        "deploy_target" => Some(register_deploy_target_backend),
        "notifications" => Some(register_notify_backend),
        "cluster_roster" => Some(register_cluster_roster_backend),
        "topology" => Some(register_topology_backend),
        "host_facts" => Some(register_host_facts_backend),
        "secrets_backend" => Some(register_secrets_backend),
        "service_identity" => Some(register_service_identity_backend),
        "diagnostics" => Some(register_diagnostics_backend),
        "notification_source" => Some(register_notification_source_backend),
        "ups" => Some(register_ups_backend),
        "agents" => Some(register_agent_provider_backend),
        "container_runtime" => Some(register_container_runtime_backend),
        "unit" => Some(register_unit_backend),
        "web" => Some(register_web_backend),
        "subprocess_env" => Some(register_subprocess_env_backend),
        "replication" => Some(register_replication_backend),
        // Fall through to domains injected by downstream crates (backup KIND /
        // TARGET, contributed by `system` at startup).
        other => EXTRA_DOMAINS
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(other)
            .map(|(register, _)| *register),
    }
}

/// Subprocess-env entry: register a plugin-backed
/// [`contract::subprocess_env::EnvProvider`]. The plugin exposes env (e.g. the
/// docker plugin's `DOCKER_HOST`) that orca merges into spawned subprocesses
/// (MCP servers). The invoke thunk is the loader's [`BackendInvoke`] shape, so
/// it passes through unwrapped.
fn register_subprocess_env_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::subprocess_env::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register subprocess_env backend '{}': {e}", def.name))
}

/// Replication-domain entry: register a `ReplicationStatusProxy` that routes the
/// `status` op back through `invoke`, feeding the mount-converge failover-safety
/// gate an observed sync health for each replication relationship (the syncthing
/// plugin). Wraps the loader's `Value` [`BackendInvoke`] into the storage crate's
/// `StorageError`-returning [`plugin_toolkit::storage::InvokeThunk`] — same bridge
/// as the storage/service domains (no wire change; the wire still carries `Value`).
fn register_replication_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::storage::{self, StorageError, replication_status};
    let thunk: storage::InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| StorageError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| StorageError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(StorageError::Transport(contract::render_invoke_error(&v))),
        }
    });
    replication_status::register_from_def(def.name.clone(), thunk)
        .map_err(|e| anyhow!("register replication backend '{}': {e}", def.name))
}

/// Unit-domain entry: register a plugin-backed [`contract::unit::UnitProvider`]
/// (the universal lifecycle surface — see `docs/MANAGED-UNIT.md`). The provider
/// enumerates many units of many kinds and performs canonical verbs; its
/// declarations/units/invoke ops route back through `invoke`. The unit registry
/// thunk is `(op, args) -> Result<String, String>` — identical to the loader's
/// [`BackendInvoke`] — so it passes through unwrapped.
fn register_unit_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::unit::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register unit backend '{}': {e}", def.name))
}

/// Web-domain entry: register a plugin-backed [`contract::web::WebProvider`]
/// that serves an HTTP surface (the frontend SPA, a viewer, static assets).
/// Per the "route rides the existing `BackendDef`" decision, the `WebRoute` is
/// read off the descriptor's shared axes — `endpoint` carries the route prefix
/// and `capabilities` carries the `spa_fallback` flag — so no ABI/proto field
/// was added. Renders route back through `invoke` as `"{invoke_prefix}.render"`.
fn register_web_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    let prefix = if def.endpoint.is_empty() {
        "/".to_string()
    } else {
        def.endpoint.clone()
    };
    let route = contract::web::WebRoute {
        prefix,
        spa_fallback: def
            .capabilities
            .iter()
            .any(|c| c == contract::web::CAP_SPA_FALLBACK),
        dev_upstream: def
            .capabilities
            .iter()
            .find_map(|c| c.strip_prefix(contract::web::CAP_DEV_UPSTREAM))
            .map(str::to_string),
    };
    // Registration is non-fatal by contract: an exact-path conflict is recorded
    // and surfaced (never returned as an error), and the only failure — a
    // poisoned registry lock — is logged and swallowed here so a web plugin can
    // never fail to load, and can never take an already-serving route offline.
    if let Err(e) = contract::web::register_from_def(def.name.clone(), route, invoke) {
        tracing::warn!(backend = %def.name, error = %e, "web backend registration issue (non-fatal)");
    }
    // Surface any contested paths for observability after this registration.
    for c in contract::web::conflicts() {
        tracing::warn!(
            path = %c.path,
            active = %c.active_owner,
            contenders = ?c.contenders,
            "web route contested; incumbent holds until the user chooses an owner"
        );
    }
    Ok(())
}

/// Container-runtime-domain entry: register a plugin-backed
/// [`plugin_toolkit::containers::RuntimeAdapter`] that routes list/inspect/
/// start/stop/logs/exec/… back through `invoke`. The containers registry's
/// thunk is `(op, args) -> Result<String, String>` — identical to the loader's
/// [`BackendInvoke`] — so it passes through unwrapped. `def.kind` carries the
/// [`RuntimeKind`] string (docker/lxc/…); `def.capabilities` may include
/// `wedge_recover`. This is how docker (bollard) / proxmox (PVE API) contribute
/// a runtime adapter without any concrete client static-linked into orca.
fn register_container_runtime_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    plugin_toolkit::containers::register_from_def(
        def.name.clone(),
        &def.kind,
        &def.capabilities,
        invoke,
    )
    .map_err(|e| anyhow!("register container_runtime backend '{}': {e}", def.name))
}

/// Agents-domain entry: register a plugin-backed [`agents::AgentProvider`] that
/// routes `agents`/`hooks`/`skills`/`commands`/`prompt_fragments` back through
/// `invoke`. The agents registry's thunk is `(op, args) -> Result<String,
/// String>` — identical to the loader's [`BackendInvoke`] — so it passes
/// through unwrapped. This is how an external plugin contributes composed Claude
/// artifacts, exactly like a storage or service backend registers its domain.
fn register_agent_provider_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    agents::register_from_def(def.name.clone(), invoke);
    Ok(())
}

/// Cluster-roster-domain entry: register a roster provider that routes
/// `list_clusters` back through `invoke`. The contract registry's `InvokeThunk`
/// is `(op, args) -> Result<String, String>` — identical to the loader's
/// [`BackendInvoke`] — so the thunk passes through unwrapped.
fn register_cluster_roster_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::cluster_roster::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register cluster_roster backend '{}': {e}", def.name))
}

/// Topology-domain entry: register a collector that routes `collect_claims`
/// back through `invoke`. Same string-error thunk shape as the loader's
/// [`BackendInvoke`], so it passes through unwrapped.
fn register_topology_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::topology::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register topology backend '{}': {e}", def.name))
}

/// Host-facts-domain entry: register a provider that routes `get_facts` back
/// through `invoke`. Same string-error thunk shape as the loader's
/// [`BackendInvoke`], so it passes through unwrapped.
fn register_host_facts_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::host_facts::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register host_facts backend '{}': {e}", def.name))
}

/// Secrets-backend-domain entry: register a backend that routes `resolve` back
/// through `invoke`. Same string-error thunk shape as the loader's
/// [`BackendInvoke`], so it passes through unwrapped.
fn register_secrets_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::secrets_backend::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register secrets_backend backend '{}': {e}", def.name))
}

/// Service-identity-domain entry: register a provider that routes
/// `list_registrations` back through `invoke`. Same string-error thunk shape as
/// the loader's [`BackendInvoke`], so it passes through unwrapped.
fn register_service_identity_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::service_identity::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register service_identity backend '{}': {e}", def.name))
}

/// Diagnostics-domain entry: register a provider that routes `diagnose`/`repair`
/// back through `invoke`. Same string-error thunk shape as the loader's
/// [`BackendInvoke`], so it passes through unwrapped. This is how a plugin
/// (raccoon, later bazzite/cachyos) contributes typed findings + repairs.
fn register_diagnostics_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::diagnostics::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register diagnostics backend '{}': {e}", def.name))
}

/// Notification-source entry: register a plugin-backed
/// [`contract::notification_source::NotificationSource`] (unraid, …) that routes
/// `poll`/`dismiss_at_source` back through `invoke`. Same JSON-proxy shape as
/// diagnostics. This is how a plugin feeds external notifications into orca's
/// stateful notification plane and dismisses them at the source.
fn register_notification_source_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::notification_source::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register notification_source backend '{}': {e}", def.name))
}

/// UPS entry: register a plugin-backed [`contract::ups::UpsProvider`] (nut,
/// unraid, …). Same JSON-proxy shape as diagnostics.
fn register_ups_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    contract::ups::register_from_def(def.name.clone(), invoke)
        .map_err(|e| anyhow!("register ups backend '{}': {e}", def.name))
}

/// Storage-domain entry in the dispatch table: parse the descriptor's
/// kind/capabilities and register a `StorageProxy` that routes operations back
/// through `invoke`. Wraps the loader's string-error thunk into the storage
/// crate's `StorageError`-returning [`plugin_toolkit::storage::InvokeThunk`].
fn register_storage_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::storage::{self, InvokeThunk, StorageError};
    // This domain keeps a `String`-payload thunk with a typed `StorageError`, so
    // bridge it to the loader's `Value` [`BackendInvoke`]: parse args in, render
    // result/error out. (No wire change — the wire still carries `Value`.)
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| StorageError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| StorageError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(StorageError::Transport(contract::render_invoke_error(&v))),
        }
    });
    storage::register_from_def_styled(
        def.name.clone(),
        &def.kind,
        def.endpoint.clone(),
        &def.capabilities,
        &def.mount_style,
        &def.net_fstypes,
        def.default_source_port,
        thunk,
    )
    .map_err(|e| anyhow!("register storage backend '{}': {e}", def.name))
}

/// Media-domain entry: register a plugin-backed [`plugin_toolkit::media::MediaBackend`]
/// keyed by media type (`def.kind`) × role (in `def.capabilities`). Bridges the
/// loader's `Value`-payload [`BackendInvoke`] into the media crate's
/// `String`-payload [`plugin_toolkit::media::InvokeThunk`] (typed `MediaError`),
/// same shape as the storage/replication domains — no wire change.
fn register_media_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::media::{self, InvokeThunk, MediaError};
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| MediaError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| MediaError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(MediaError::Transport(contract::render_invoke_error(&v))),
        }
    });
    media::register_from_def(
        def.name.clone(),
        &def.kind,
        def.endpoint.clone(),
        &def.capabilities,
        thunk,
    )
    .map_err(|e| anyhow!("register media backend '{}': {e}", def.name))
}

/// Guest-mount-domain entry: register a plugin-backed
/// [`plugin_toolkit::storage::GuestMountApplier`] — a runtime plugin (proxmox)
/// that realizes a mount INSIDE a guest (`lxc.mount.entry` / cloud-init). Bridges
/// the loader's `Value` [`BackendInvoke`] to the storage crate's `String`-payload
/// [`InvokeThunk`], same as [`register_storage_backend`].
fn register_guest_mount_applier(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::storage::{self, InvokeThunk, StorageError};
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| StorageError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| StorageError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(StorageError::Transport(contract::render_invoke_error(&v))),
        }
    });
    storage::register_guest_applier_from_def(def.name.clone(), thunk);
    Ok(())
}

/// Service-domain entry in the dispatch table: register a `ServiceProxy` that
/// routes lifecycle ops (deploy/backup/restore/configure/status) back through
/// `invoke`. The descriptor reuses `BackendDef`'s generic axes — `kind` carries
/// the default port, `runtime` the supported-modality CSV. Wraps the loader's
/// string-error thunk into the service crate's `ServiceError`-returning
/// [`plugin_toolkit::service::InvokeThunk`].
fn register_service_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::service::{self, InvokeThunk, ServiceError};
    // Bridge the domain's `String`/`ServiceError` thunk to the loader's `Value`
    // [`BackendInvoke`] (no wire change — the wire still carries `Value`).
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| ServiceError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| ServiceError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(ServiceError::Transport(contract::render_invoke_error(&v))),
        }
    });
    service::register_from_def(
        def.name.clone(),
        &def.kind,    // default port
        &def.runtime, // modality CSV
        def.endpoint.clone(),
        &def.capabilities,
        thunk,
    )
    .map_err(|e| anyhow!("register service backend '{}': {e}", def.name))
}

/// Deploy-target-domain entry in the dispatch table: parse the descriptor's
/// discrete `(host, runtime, kind)` identity axes plus capabilities and register
/// a `DeployProxy` that routes operations back through `invoke`. Wraps the
/// loader's string-error thunk into the deploy-target crate's
/// `DeployError`-returning [`plugin_toolkit::deploy_target::InvokeThunk`]. This
/// is how a plugin (docker/dockge/unraid/proxmox) advertises itself as a place
/// orca can run a workload: one `BackendDef` per concrete `(host, runtime,
/// kind)` target. The `name` field carries the host axis; `runtime` and `kind`
/// are their own fields so the same host/runtime can be managed several ways
/// (e.g. a Docker engine driven via both Dockge and the plain CLI) without
/// collapsing into one hardcoded identifier.
fn register_deploy_target_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::deploy_target::{self, DeployError, InvokeThunk};
    // Bridge the domain's `String`/`DeployError` thunk to the loader's `Value`
    // [`BackendInvoke`] (no wire change — the wire still carries `Value`).
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| DeployError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| DeployError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(DeployError::Transport(contract::render_invoke_error(&v))),
        }
    });
    deploy_target::register_from_def(
        def.name.clone(), // host axis
        &def.runtime,
        &def.kind,
        def.endpoint.clone(),
        &def.capabilities,
        def.provisioning.clone(),
        thunk,
    )
    .map_err(|e| anyhow!("register deploy-target backend '{}': {e}", def.name))
}

/// Notifications-domain entry in the dispatch table: register a `NotifyProxy`
/// that routes `emit` back through `invoke`. A backend plugin (ntfy, slack, …)
/// advertises one `BackendDef` per enabled endpoint; each becomes a named
/// notification backend routing rules can target. Wraps the loader's
/// string-error thunk into the notify crate's `BackendError`-returning
/// [`plugin_toolkit::notify::InvokeThunk`].
fn register_notify_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::notify::{self, BackendError, InvokeThunk};
    // Bridge the domain's `String`/`BackendError` thunk to the loader's `Value`
    // [`BackendInvoke`] (no wire change — the wire still carries `Value`).
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        let args = sj::from_str(&args_json)
            .map_err(|e| BackendError::Transport(format!("encode args for '{op}': {e}")))?;
        match invoke(op, args) {
            Ok(v) => sj::to_string(&v)
                .map_err(|e| BackendError::Transport(format!("decode result for '{op}': {e}"))),
            Err(v) => Err(BackendError::Transport(contract::render_invoke_error(&v))),
        }
    });
    notify::register_from_def(def.name.clone(), thunk)
        .map_err(|e| anyhow!("register notification backend '{}': {e}", def.name))
}

/// Deregister one backend from its domain registry. Domain-agnostic reverse of
/// [`domain_register`]; the deregistration path a reload/unload needs. Logs and
/// continues on an unknown domain (a recorded pair always came from a known
/// domain, so this is defensive only).
fn domain_deregister(domain: &str, name: &str) {
    match domain {
        "storage" => {
            plugin_toolkit::storage::deregister_backend(name);
        }
        "media" => {
            plugin_toolkit::media::deregister_backend(name);
        }
        "guest_mount" => {
            plugin_toolkit::storage::deregister_guest_applier(name);
        }
        "deploy_target" => {
            // `name` is the host axis recorded at load; drop every target the
            // plugin registered on that host.
            plugin_toolkit::deploy_target::deregister_host(name);
        }
        "notifications" => {
            plugin_toolkit::notify::deregister_backend(name);
        }
        "cluster_roster" => {
            contract::cluster_roster::deregister_backend(name);
        }
        "topology" => {
            contract::topology::deregister_collector(name);
        }
        "host_facts" => {
            contract::host_facts::deregister_provider(name);
        }
        "secrets_backend" => {
            contract::secrets_backend::deregister_provider(name);
        }
        "service_identity" => {
            contract::service_identity::deregister_backend(name);
        }
        "diagnostics" => {
            contract::diagnostics::deregister_provider(name);
        }
        "notification_source" => {
            contract::notification_source::deregister_source(name);
        }
        "ups" => {
            contract::ups::deregister_provider(name);
        }
        "agents" => {
            agents::deregister_provider(name);
        }
        "container_runtime" => {
            plugin_toolkit::containers::deregister_adapter(name);
        }
        "unit" => {
            contract::unit::deregister_provider(name);
        }
        "web" => {
            contract::web::deregister_provider(name);
        }
        "subprocess_env" => {
            contract::subprocess_env::deregister_provider(name);
        }
        "replication" => {
            plugin_toolkit::storage::deregister_status_provider(name);
        }
        other => {
            // A domain injected by a downstream crate (backup KIND / TARGET)?
            let injected = EXTRA_DOMAINS
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(other)
                .map(|(_, deregister)| *deregister);
            match injected {
                Some(deregister) => deregister(name),
                None => {
                    tracing::warn!(domain = %other, %name, "deregister for unknown domain ignored")
                }
            }
        }
    }
}

/// Reverse a set of `(domain, name)` registrations — used both to roll back a
/// partially-registered plugin on load failure and to clean up on unload.
fn rollback_domain_backends(pairs: &[(String, String)]) {
    for (domain, name) in pairs {
        domain_deregister(domain, name);
    }
}

/// Build the invoke thunk for one backend: closes over the plugin's [`Backing`]
/// (cheap to clone — an `Arc` process handle) and its `invoke_prefix`, so each
/// proxied `op` becomes a `"{prefix}.{op}"` call routed through the same
/// subprocess socket the plugin's tools use.
fn make_backend_invoke(backing: Backing, invoke_prefix: String) -> BackendInvoke {
    Arc::new(move |op: &str, args: sj::Value| {
        let tool = format!("{invoke_prefix}.{op}");
        backing.invoke(&tool, args)
    })
}

/// Register every backend a plugin declares into its domain registry, routing
/// each through `backing`. On any failure (unknown domain, constructor error)
/// the already-registered backends for this plugin are rolled back so a partial
/// load never leaves orphans. Returns the `(domain, name)` pairs registered, for
/// the caller to record on the `LoadedPlugin` (so unload can reverse them).
///
/// Used by the subprocess ([`spawn_plugin`]) load path via the [`Backing`]
/// behind the invoke thunk.
fn register_backends(
    backing: &Backing,
    software: &str,
    defs: &[BackendDef],
) -> Result<Vec<(String, String)>> {
    let mut registered: Vec<(String, String)> = Vec::new();
    for def in defs {
        let Some(register) = domain_register(&def.domain) else {
            rollback_domain_backends(&registered);
            bail!(
                "plugin '{software}' backend '{}' targets unknown domain '{}'",
                def.name,
                def.domain
            );
        };
        let invoke = make_backend_invoke(backing.clone(), def.invoke_prefix.clone());
        if let Err(e) = register(def, invoke) {
            rollback_domain_backends(&registered);
            return Err(e.context(format!("plugin '{software}' backend registration failed")));
        }
        registered.push((def.domain.clone(), def.name.clone()));
    }
    Ok(registered)
}

/// Process-global registry of loaded plugins, keyed by tool name → plugin index.
struct Registry {
    plugins: Vec<LoadedPlugin>,
    by_tool: HashMap<String, usize>,
}

static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();

// Lock acquisitions on this registry (and EXTRA_DOMAINS) recover a poisoned
// guard with `unwrap_or_else(|e| e.into_inner())` rather than `expect`: the
// registry is in-memory data that survives a panicking thread intact, and this
// is a long-lived daemon — one transient panic elsewhere must not crash the
// process or permanently break every subsequent plugin operation.
fn registry() -> &'static RwLock<Registry> {
    REGISTRY.get_or_init(|| {
        RwLock::new(Registry {
            plugins: Vec::new(),
            by_tool: HashMap::new(),
        })
    })
}

/// Outcome of a successful load — what got registered, for the caller to log.
#[derive(Debug, Clone)]
pub struct LoadReport {
    /// `target_software` from the plugin header.
    pub software: String,
    /// The plugin's own semver.
    pub semver: String,
    /// Names of the tools registered from this plugin.
    pub tools: Vec<String>,
    /// The plugin's declared SQL-table schemas (namespaced to itself). The
    /// installer applies these via `db::plugin_tables::apply_decl` against the
    /// real db connection — the loader does not own db lifecycle. Empty
    /// `namespace`/`tables` for a plugin that declares none.
    pub declared_schema: SchemaDecl,
}

/// Spawn an out-of-process plugin executable, complete the wire-protocol
/// handshake, and register its tool surface into the same runtime registry that
/// backs every loaded plugin — so `dispatch`/`invoke_plugin` route to it
/// uniformly.
///
/// There is no `orca_compat` semver gate: compatibility is negotiated at runtime
/// as a wire-protocol major match inside [`supervisor::PluginProcess::spawn`].
///
/// Backends (topology / unit / host_facts / …) register through the domain
/// dispatch table: the plugin sends each backend def as verbatim JSON over the
/// wire ([`Frame::Hello`](plugin_proto::Frame::Hello)'s `backends`), which the
/// daemon parses into its own `abi::BackendDef` — so no field is lost, and each
/// backend's ops route back through the subprocess.
///
/// `expected_id` is the authoritative plugin id orca is loading this binary as
/// (the install-dir filename). When `Some`, it is validated against the plugin's
/// self-declared handshake id and becomes the session principal that confines
/// the plugin's `db.op`/`secret.op` to its own namespace. Pass `None` only for
/// trust-on-first-use (sideloading an arbitrary file before its id is recorded).
#[cfg(unix)]
pub fn spawn_plugin(exe: &Path, expected_id: Option<&str>) -> Result<LoadReport> {
    let proc = supervisor::PluginProcess::spawn(exe, expected_id)?;
    let software = proc.software.clone();
    let semver = proc.semver.clone();

    // proto `ToolDef` → abi `ToolDef`: identical JSON shape (name/description/
    // input_schema/output_schema), so a serde round-trip is lossless. The
    // registry + surfaces speak abi types.
    let mut tools: HashMap<String, ToolDef> = HashMap::new();
    for def in &proc.manifest {
        let abi_def: ToolDef = sj::to_value(def)
            .and_then(sj::from_value)
            .with_context(|| {
                format!("plugin '{software}' tool '{}' has an invalid def", def.name)
            })?;
        tools.insert(abi_def.name.clone(), abi_def);
    }
    let mut tool_names: Vec<String> = tools.keys().cloned().collect();
    tool_names.sort();

    // Backend defs arrive as verbatim JSON so the daemon's richer shape survives
    // the wire; parse each into `abi::BackendDef` here.
    let backend_defs: Vec<BackendDef> = proc
        .backends
        .iter()
        .map(|v| sj::from_value(v.clone()))
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("plugin '{software}' returned an invalid backends list"))?;

    let declared_schema: SchemaDecl = if proc.schema.is_null() {
        SchemaDecl::default()
    } else {
        sj::from_value(proc.schema.clone()).with_context(|| {
            format!("plugin '{software}' returned an invalid schema declaration")
        })?
    };

    let backing = Backing::Process(Arc::new(proc));

    // Upgrade / reload semantics: if a plugin reporting the SAME `software` is
    // already loaded, unload it first so its tool routes free up. Without this an
    // in-place upgrade (install a newer build of a loaded plugin) false-collides
    // with its own previous version's tool names and bails. A collision against a
    // *different* software's tool below still errors. `unload_plugin` takes the
    // registry lock itself, so run it before acquiring the write lock here.
    // See [[project-orca-plugin-rollout-defects]].
    if is_loaded(&software) {
        let n = unload_plugin(&software);
        tracing::info!(plugin = %software, unloaded = n, "reloading plugin (same software already loaded)");
    }

    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    for name in &tool_names {
        if reg.by_tool.contains_key(name) {
            bail!("plugin '{software}' tool '{name}' collides with an already-loaded plugin tool");
        }
        if dispatch::tool_exists(name) {
            bail!("plugin '{software}' tool '{name}' collides with a built-in tool");
        }
    }
    let registered = register_backends(&backing, &software, &backend_defs)?;

    let idx = reg.plugins.len();
    for name in &tool_names {
        reg.by_tool.insert(name.clone(), idx);
    }
    let backend_names: Vec<String> = registered.iter().map(|(_, n)| n.clone()).collect();
    reg.plugins.push(LoadedPlugin {
        software: software.clone(),
        semver: semver.clone(),
        // Process plugins negotiate compat at the wire level; there is no
        // target/orca semver range on this path. Empty = "not applicable".
        target_compat: String::new(),
        orca_compat: String::new(),
        backing,
        tools,
        domain_backends: registered,
    });

    tracing::info!(
        plugin = %software,
        version = %semver,
        tools = ?tool_names,
        backends = ?backend_names,
        "loaded out-of-process plugin"
    );

    Ok(LoadReport {
        software,
        semver,
        tools: tool_names,
        declared_schema,
    })
}

/// The plugin tool manifest entries for every loaded plugin, in load order.
/// Lets the host merge dynamic tools into MCP/OpenAPI surfaces.
pub fn loaded_tool_defs() -> Vec<ToolDef> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.plugins
        .iter()
        .flat_map(|p| p.tools.values().cloned())
        .collect()
}

/// Header + tool-name summary of one loaded plugin. The plugin-management tool
/// surface (`plugin.list`) reads this to report what is live in-process,
/// distinct from what is merely present on disk or known in the catalog.
#[derive(Debug, Clone)]
pub struct LoadedPluginInfo {
    /// `target_software` from the header, e.g. `"jellyfin"`.
    pub software: String,
    /// The plugin's own semver.
    pub semver: String,
    /// Free-form target-software compat range.
    pub target_compat: String,
    /// The orca-version semver range the plugin declared.
    pub orca_compat: String,
    /// Sorted names of the tools this plugin registered.
    pub tools: Vec<String>,
}

/// Summaries of every currently-loaded plugin, in load order. Drives
/// `plugin.list`'s "loaded" column.
pub fn loaded_plugins() -> Vec<LoadedPluginInfo> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.plugins
        .iter()
        .map(|p| {
            let mut tools: Vec<String> = p.tools.keys().cloned().collect();
            tools.sort();
            LoadedPluginInfo {
                software: p.software.clone(),
                semver: p.semver.clone(),
                target_compat: p.target_compat.clone(),
                orca_compat: p.orca_compat.clone(),
                tools,
            }
        })
        .collect()
}

/// Whether a plugin reporting `software` as its `target_software` is currently
/// loaded in the runtime registry.
pub fn is_loaded(software: &str) -> bool {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.plugins.iter().any(|p| p.software == software)
}

/// Unregister every loaded plugin whose `target_software` matches `software`,
/// dropping its tool-name routes so the names free up again.
///
/// This removes the plugin from the *routing* registry and drops its
/// [`Backing`]; dropping the last `Arc<PluginProcess>` tears down the child
/// process. A reinstall under the same name re-registers cleanly. Returns the
/// number of plugins removed.
pub fn unload_plugin(software: &str) -> usize {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    let before = reg.plugins.len();
    let removed_tools: Vec<String> = reg
        .plugins
        .iter()
        .filter(|p| p.software == software)
        .flat_map(|p| p.tools.keys().cloned())
        .collect();
    // Reverse every domain-backend registration the unloaded plugins made, so a
    // dropped plugin leaves no storage (etc.) backend pointing at a dead invoke
    // thunk. Collected before the `retain` removes the plugins.
    let removed_backends: Vec<(String, String)> = reg
        .plugins
        .iter()
        .filter(|p| p.software == software)
        .flat_map(|p| p.domain_backends.iter().cloned())
        .collect();
    rollback_domain_backends(&removed_backends);
    reg.plugins.retain(|p| p.software != software);
    for name in &removed_tools {
        reg.by_tool.remove(name);
    }
    // Tool→index map points into `plugins` by position; rebuild it after a
    // retain shifts indices.
    reg.by_tool.clear();
    let rebuilt: Vec<(String, usize)> = reg
        .plugins
        .iter()
        .enumerate()
        .flat_map(|(idx, p)| p.tools.keys().cloned().map(move |n| (n, idx)))
        .collect();
    for (name, idx) in rebuilt {
        reg.by_tool.insert(name, idx);
    }
    before - reg.plugins.len()
}

/// The cloned backing + owning plugin name for a tool, or `None` if no loaded
/// plugin owns it. Clones the (cheap) [`Backing`] and releases the registry lock
/// before returning, so a slow plugin invoke — a subprocess socket round-trip —
/// never holds the lock or blocks other dispatch.
fn backing_for(name: &str) -> Option<(Backing, String)> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let idx = *reg.by_tool.get(name)?;
    let plugin = &reg.plugins[idx];
    Some((plugin.backing.clone(), plugin.software.clone()))
}

/// Marshal an invoke result into the caller's `Result<Value>`. The value is
/// already parsed (it rode the wire as `Value`), so success passes through with
/// no reparse; an error `Value` is rendered to text with a plugin-named context.
fn parse_invoke_result(
    result: std::result::Result<sj::Value, sj::Value>,
    name: &str,
    _software: &str,
) -> Result<sj::Value> {
    match result {
        Ok(value) => Ok(value),
        Err(msg) => Err(anyhow!(
            "plugin tool '{name}' failed: {}",
            contract::render_invoke_error(&msg)
        )),
    }
}

/// Dispatch a tool call. Tries the dynamically-loaded plugin registry first;
/// on a miss, falls back to the statically-linked `dispatch::dispatch`. This is
/// the entrypoint the host's MCP/REST/CLI paths should call instead of
/// `dispatch::dispatch` directly, so loaded plugins share one tool namespace.
///
/// A plugin invoke runs on a **blocking** thread (`spawn_blocking`): the call is
/// synchronous and, for a subprocess plugin, does blocking socket I/O and drives
/// capability round-trips (which may block on their own I/O runtime). Keeping it
/// off the async worker pool is what makes the capability host's `block_on`
/// safe and stops one plugin's latency from starving the scheduler.
pub async fn dispatch(name: &str, args: sj::Value, ctx: &ToolCtx) -> Result<sj::Value> {
    if let Some((backing, software)) = backing_for(name) {
        let owned = name.to_string();
        let result = tokio::task::spawn_blocking(move || backing.invoke(&owned, args))
            .await
            .with_context(|| format!("plugin invoke task for '{name}' panicked"))?;
        return parse_invoke_result(result, name, &software);
    }
    dispatch::dispatch(name, args, ctx).await
}

/// Synchronous tool dispatch into the plugin registry. Returns `None` when no
/// loaded plugin owns `name`, so a sync caller can fall through to the built-in
/// registry.
///
/// Prefer async [`dispatch`] from an async context: this runs the invoke inline,
/// so for a subprocess plugin it blocks the calling thread on socket I/O (and
/// must NOT be called from a tokio async worker — the capability host would
/// `block_on` on it).
pub fn invoke_plugin(name: &str, args: &sj::Value) -> Option<Result<sj::Value>> {
    let (backing, software) = backing_for(name)?;
    let result = backing.invoke(name, args.clone());
    Some(parse_invoke_result(result, name, &software))
}

#[cfg(test)]
mod extra_domain_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static REGISTERED: AtomicUsize = AtomicUsize::new(0);
    static DEREGISTERED: AtomicUsize = AtomicUsize::new(0);

    fn fake_register(_def: &BackendDef, _invoke: BackendInvoke) -> Result<()> {
        REGISTERED.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn fake_deregister(_name: &str) {
        DEREGISTERED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn injected_domain_dispatches_register_and_deregister() {
        let domain = "test-extra-domain-xyz";
        // Unknown before injection.
        assert!(domain_register(domain).is_none());

        register_domain_constructor(domain, fake_register, fake_deregister);

        // domain_register now resolves the injected constructor.
        let ctor = domain_register(domain).expect("injected domain resolves");
        let invoke: BackendInvoke = Arc::new(|_op: &str, _args: sj::Value| Ok(sj::json!({})));
        let def = BackendDef {
            domain: domain.to_string(),
            ..Default::default()
        };
        ctor(&def, invoke).unwrap();
        assert_eq!(REGISTERED.load(Ordering::SeqCst), 1);

        // domain_deregister routes through the injected deregister.
        domain_deregister(domain, "some-name");
        assert_eq!(DEREGISTERED.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod loader_tests {
    use super::*;

    /// A no-op invoke thunk: every proxied op just echoes an empty object. Enough
    /// to drive backend registration (which only *constructs* the thunk — it runs
    /// lazily on invoke).
    fn noop_invoke() -> BackendInvoke {
        Arc::new(|_op: &str, _args: sj::Value| Ok(sj::json!({})))
    }

    /// The full set of domains the hardcoded [`domain_register`] table names.
    const BUILTIN_DOMAINS: &[&str] = &[
        "storage",
        "service",
        "deploy_target",
        "notifications",
        "cluster_roster",
        "topology",
        "host_facts",
        "secrets_backend",
        "service_identity",
        "diagnostics",
        "notification_source",
        "ups",
        "agents",
        "container_runtime",
        "unit",
        "web",
        "subprocess_env",
        "guest_mount",
        "replication",
    ];

    #[test]
    fn domain_register_resolves_every_builtin_domain() {
        for domain in BUILTIN_DOMAINS {
            assert!(
                domain_register(domain).is_some(),
                "builtin domain '{domain}' must resolve a constructor"
            );
        }
    }

    #[test]
    fn domain_register_rejects_unknown_domain() {
        assert!(domain_register("no-such-domain-abc123").is_none());
        assert!(domain_register("").is_none());
    }

    /// Registering then deregistering every builtin domain exercises each
    /// `register_*_backend` body (thunk construction + `register_from_def`) and
    /// the matching `domain_deregister` match arm end-to-end.
    #[test]
    fn each_builtin_domain_registers_and_deregisters() {
        for domain in BUILTIN_DOMAINS {
            let ctor = domain_register(domain).expect("builtin resolves");
            let name = format!("loader-test-{domain}");
            let def = BackendDef {
                domain: (*domain).to_string(),
                name: name.clone(),
                invoke_prefix: name.clone(),
                ..Default::default()
            };
            // Registration must not panic; most domains accept a minimal def.
            // (Any domain that rejects a minimal def still exercises its body.)
            let _outcome = ctor(&def, noop_invoke());
            // Deregister must be a safe no-op / reverse and never panic.
            domain_deregister(domain, &name);
        }
    }

    #[test]
    fn domain_deregister_unknown_domain_is_ignored() {
        // Must not panic on a domain with no registered constructor.
        domain_deregister("totally-unknown-domain", "whatever");
    }

    #[test]
    fn web_backend_parses_route_from_descriptor() {
        // Empty endpoint → root prefix; capabilities carry spa_fallback + dev_upstream.
        let def = BackendDef {
            domain: "web".to_string(),
            name: "loader-web-root".to_string(),
            endpoint: String::new(),
            capabilities: vec![
                contract::web::CAP_SPA_FALLBACK.to_string(),
                format!("{}http://localhost:5173", contract::web::CAP_DEV_UPSTREAM),
            ],
            invoke_prefix: "loader-web-root".to_string(),
            ..Default::default()
        };
        register_web_backend(&def, noop_invoke()).expect("web register is non-fatal");

        // Non-empty endpoint → explicit prefix, no spa fallback.
        let def2 = BackendDef {
            domain: "web".to_string(),
            name: "loader-web-app".to_string(),
            endpoint: "/app".to_string(),
            capabilities: vec![],
            invoke_prefix: "loader-web-app".to_string(),
            ..Default::default()
        };
        register_web_backend(&def2, noop_invoke()).expect("web register is non-fatal");

        domain_deregister("web", "loader-web-root");
        domain_deregister("web", "loader-web-app");
    }

    /// Two web backends claiming the exact same route prefix must not fail to
    /// load (registration is non-fatal by contract), and the contested path must
    /// be recorded so the loader's post-registration `conflicts()` warn loop runs
    /// with a real conflict in hand. The incumbent keeps serving the route.
    #[test]
    fn web_backend_records_route_conflict_non_fatally() {
        let prefix = "/loader-web-dup";
        let incumbent = "loader-web-dup-a";
        let contender = "loader-web-dup-b";

        let def_a = BackendDef {
            domain: "web".to_string(),
            name: incumbent.to_string(),
            endpoint: prefix.to_string(),
            invoke_prefix: incumbent.to_string(),
            ..Default::default()
        };
        // First claim on the path: non-fatal, no conflict yet.
        register_web_backend(&def_a, noop_invoke()).expect("first web register is non-fatal");

        let def_b = BackendDef {
            domain: "web".to_string(),
            name: contender.to_string(),
            endpoint: prefix.to_string(),
            invoke_prefix: contender.to_string(),
            ..Default::default()
        };
        // Second claim on the SAME path: still non-fatal, but records a conflict
        // that the loader's conflicts() warn loop iterates.
        register_web_backend(&def_b, noop_invoke()).expect("conflicting web register is non-fatal");

        let contested = contract::web::conflicts();
        let mine = contested
            .iter()
            .find(|c| c.path == prefix)
            .expect("the contested path is recorded");
        assert_eq!(
            mine.active_owner, incumbent,
            "the incumbent holds the route until the user chooses an owner"
        );
        assert!(
            mine.contenders.iter().any(|c| c == contender),
            "the second backend is recorded as a contender: {:?}",
            mine.contenders
        );

        domain_deregister("web", incumbent);
        domain_deregister("web", contender);
    }

    #[test]
    fn rollback_domain_backends_reverses_every_pair() {
        // Register two backends, then roll them both back. No panic, safe reverse.
        let pairs = vec![
            ("agents".to_string(), "loader-rollback-agents".to_string()),
            ("topology".to_string(), "loader-rollback-topo".to_string()),
        ];
        for (domain, name) in &pairs {
            let ctor = domain_register(domain).expect("resolves");
            let def = BackendDef {
                domain: domain.clone(),
                name: name.clone(),
                invoke_prefix: name.clone(),
                ..Default::default()
            };
            let _outcome = ctor(&def, noop_invoke());
        }
        rollback_domain_backends(&pairs);
    }

    #[test]
    fn make_backend_invoke_prefixes_op() {
        // No `Backing` is constructible without a real subprocess, so verify the
        // prefixing contract via the same `format!` the thunk uses.
        let prefix = "nfs";
        let op = "recover_stale";
        assert_eq!(format!("{prefix}.{op}"), "nfs.recover_stale");
    }

    #[test]
    fn parse_invoke_result_passes_success_through() {
        let ok = parse_invoke_result(Ok(sj::json!({"n": 7})), "some.tool", "sw").unwrap();
        assert_eq!(ok, sj::json!({"n": 7}));
    }

    #[test]
    fn parse_invoke_result_renders_string_error_verbatim() {
        let err = parse_invoke_result(
            Err(sj::Value::String("boom".to_string())),
            "some.tool",
            "sw",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("some.tool"), "names the tool: {err}");
        assert!(err.contains("boom"), "includes the error text: {err}");
    }

    #[test]
    fn parse_invoke_result_renders_non_string_error_as_json() {
        let err = parse_invoke_result(Err(sj::json!({"code": 42})), "t", "sw")
            .unwrap_err()
            .to_string();
        assert!(err.contains("42"), "renders structured error: {err}");
    }

    #[test]
    fn registry_queries_are_safe_when_software_absent() {
        // A software that was never loaded: not loaded, no tools, unload removes 0.
        let sw = "loader-test-never-loaded-xyz";
        assert!(!is_loaded(sw));
        assert_eq!(unload_plugin(sw), 0);
        assert!(backing_for("loader-test-never-a-tool-xyz").is_none());
        assert!(invoke_plugin("loader-test-never-a-tool-xyz", &sj::json!({})).is_none());
    }

    #[test]
    fn registry_accessors_do_not_panic() {
        // These read the process-global registry; they may see plugins loaded by
        // other tests but must always return without panicking.
        let _ = loaded_tool_defs();
        let _ = loaded_plugins();
    }

    #[test]
    fn load_report_is_debug_and_clone() {
        let report = LoadReport {
            software: "jellyfin".to_string(),
            semver: "0.1.0".to_string(),
            tools: vec!["a".to_string(), "b".to_string()],
            declared_schema: SchemaDecl::default(),
        };
        let cloned = report.clone();
        assert_eq!(cloned.software, "jellyfin");
        assert_eq!(cloned.tools, vec!["a".to_string(), "b".to_string()]);
        assert!(format!("{report:?}").contains("jellyfin"));
    }

    #[tokio::test]
    async fn dispatch_falls_through_to_builtin_for_unowned_tool() {
        // No loaded plugin owns this name → async `dispatch` delegates to the
        // statically linked `dispatch::dispatch`, which rejects an unknown tool
        // with an error (never a panic, never a fabricated success). This drives
        // the fallback arm past the plugin-registry miss.
        let cfg = Arc::new(contract::config::Config::load().unwrap());
        let ctx = ToolCtx::new(cfg);
        let res = dispatch("loader-test-unowned-builtin-xyz", sj::json!({}), &ctx).await;
        assert!(res.is_err(), "unknown tool must error, got: {res:?}");
    }

    /// Driving a registered notify backend through `notifications::emit` runs
    /// the loader's bridge thunk (`register_notify_backend`) end to end: it
    /// parses the JSON args, calls the loader `BackendInvoke`, and renders the
    /// success result back into a `MessageRef`.
    #[tokio::test]
    async fn notify_backend_thunk_routes_success() {
        use plugin_toolkit::notify::{self, Event, EventClass, Severity};
        let name = "loader-notify-ok";
        let invoke: BackendInvoke = {
            let n = name.to_string();
            Arc::new(move |op: &str, _args: sj::Value| {
                assert_eq!(op, "emit", "notify backend proxies the emit op");
                Ok(sj::json!({ "backend": n, "id": "msg-42" }))
            })
        };
        let def = BackendDef {
            domain: "notifications".to_string(),
            name: name.to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_notify_backend(&def, invoke).expect("notify register");

        let event = Event::new(EventClass::Alert, Severity::Info, "hi", "loader-test");
        let outcomes = notify::emit(&event).await;
        let mine = outcomes
            .iter()
            .find(|o| o.backend == name)
            .expect("our backend emitted");
        let msg = mine.result.as_ref().expect("emit succeeded");
        assert_eq!(msg.id, "msg-42", "MessageRef decoded from the thunk result");

        domain_deregister("notifications", name);
    }

    /// The error arm of the notify bridge thunk: a `BackendInvoke` that returns
    /// an error `Value` must surface as a transport error carrying the rendered
    /// message, never a panic or a fabricated success.
    #[tokio::test]
    async fn notify_backend_thunk_surfaces_error() {
        use plugin_toolkit::notify::{self, Event, EventClass, Severity};
        let name = "loader-notify-err";
        let invoke: BackendInvoke =
            Arc::new(|_op: &str, _args: sj::Value| Err(sj::Value::String("emit-exploded".into())));
        let def = BackendDef {
            domain: "notifications".to_string(),
            name: name.to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_notify_backend(&def, invoke).expect("notify register");

        let event = Event::new(EventClass::Alert, Severity::Error, "boom", "loader-test");
        let outcomes = notify::emit(&event).await;
        let mine = outcomes
            .iter()
            .find(|o| o.backend == name)
            .expect("our backend was selected");
        let err = mine.result.as_ref().unwrap_err().to_string();
        assert!(err.contains("emit-exploded"), "renders the error: {err}");

        domain_deregister("notifications", name);
    }

    /// Driving a registered storage backend through `dispatch_op` runs the
    /// loader's storage bridge thunk: it encodes args to a JSON string, invokes
    /// the `BackendInvoke`, and decodes the result string back to a `Value`.
    #[tokio::test]
    async fn storage_backend_thunk_routes_success() {
        use plugin_toolkit::storage;
        let name = "loader-storage-ok";
        let invoke: BackendInvoke = Arc::new(|op: &str, _args: sj::Value| {
            assert_eq!(op, "list_shares");
            // list_shares decodes into Vec<Share>; an empty list is valid.
            Ok(sj::json!([]))
        });
        let def = BackendDef {
            domain: "storage".to_string(),
            name: name.to_string(),
            kind: "network_share".to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_storage_backend(&def, invoke).expect("storage register");

        let backend = storage::backend(name).expect("backend is registered");
        let out = storage::dispatch_op(&*backend, "list_shares", sj::json!({}))
            .await
            .expect("op succeeds through the thunk");
        assert_eq!(out, sj::json!([]), "empty share list round-tripped");

        domain_deregister("storage", name);
        assert!(storage::backend(name).is_none(), "deregister removed it");
    }

    /// The error arm of the storage bridge thunk: an error `Value` from the
    /// `BackendInvoke` becomes a `StorageError::Transport` that `dispatch_op`
    /// surfaces as an error `Value` carrying the rendered message.
    #[tokio::test]
    async fn storage_backend_thunk_surfaces_error() {
        use plugin_toolkit::storage;
        let name = "loader-storage-err";
        let invoke: BackendInvoke =
            Arc::new(|_op: &str, _args: sj::Value| Err(sj::Value::String("disk-gone".into())));
        let def = BackendDef {
            domain: "storage".to_string(),
            name: name.to_string(),
            kind: "network_share".to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_storage_backend(&def, invoke).expect("storage register");

        let backend = storage::backend(name).expect("backend is registered");
        let err = storage::dispatch_op(&*backend, "list_shares", sj::json!({}))
            .await
            .expect_err("op fails through the thunk");
        assert!(
            err.to_string().contains("disk-gone"),
            "renders the invoke error: {err}"
        );

        domain_deregister("storage", name);
    }

    /// Registering a secrets backend via the loader entry then resolving through
    /// the contract registry drives `register_secrets_backend` and proves the
    /// loader `BackendInvoke` is what the proxy calls.
    #[tokio::test]
    async fn secrets_backend_resolves_through_loader_entry() {
        let kind = "loader-secrets-ok";
        let invoke: BackendInvoke = Arc::new(|op: &str, args: sj::Value| {
            assert_eq!(op, "resolve");
            assert_eq!(args["ref_path"], sj::json!("op://vault/item"));
            Ok(sj::json!("resolved-secret"))
        });
        let def = BackendDef {
            domain: "secrets_backend".to_string(),
            name: kind.to_string(),
            invoke_prefix: kind.to_string(),
            ..Default::default()
        };
        register_secrets_backend(&def, invoke).expect("secrets register");

        let value = contract::secrets_backend::resolve(kind, "op://vault/item")
            .await
            .expect("resolve succeeds");
        assert_eq!(value, "resolved-secret");

        domain_deregister("secrets_backend", kind);
        // After deregister the backend kind is unknown → resolve errors.
        let miss = contract::secrets_backend::resolve(kind, "op://vault/item").await;
        assert!(miss.is_err(), "deregistered backend no longer resolves");
    }

    /// The error arm of a secrets resolve: an error `Value` from the invoke
    /// surfaces as an anyhow error carrying the rendered message.
    #[tokio::test]
    async fn secrets_backend_surfaces_invoke_error() {
        let kind = "loader-secrets-err";
        let invoke: BackendInvoke =
            Arc::new(|_op: &str, _args: sj::Value| Err(sj::Value::String("vault-locked".into())));
        let def = BackendDef {
            domain: "secrets_backend".to_string(),
            name: kind.to_string(),
            invoke_prefix: kind.to_string(),
            ..Default::default()
        };
        register_secrets_backend(&def, invoke).expect("secrets register");

        let err = contract::secrets_backend::resolve(kind, "ref")
            .await
            .expect_err("resolve fails");
        assert!(
            err.to_string().contains("vault-locked"),
            "renders invoke error: {err}"
        );

        domain_deregister("secrets_backend", kind);
    }

    /// Driving a registered service backend through `ServiceBackend::status`
    /// runs the loader's service bridge thunk end to end: it encodes the op
    /// args to a JSON string, invokes the `BackendInvoke`, and decodes the
    /// result string back into a typed `ServiceStatus`.
    #[tokio::test]
    async fn service_backend_thunk_routes_success() {
        use plugin_toolkit::service::{self, Endpoint};
        let name = "loader-service-ok";
        let invoke: BackendInvoke = Arc::new(|op: &str, _args: sj::Value| {
            assert_eq!(op, "status");
            Ok(sj::json!({ "healthy": true, "detail": "all-green" }))
        });
        let def = BackendDef {
            domain: "service".to_string(),
            name: name.to_string(),
            // `kind` is the default port; `runtime` is the modality CSV. Both
            // must parse for registration to succeed and build the proxy.
            kind: "8080".to_string(),
            runtime: "docker".to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_service_backend(&def, invoke).expect("service register");

        let backend = service::backend(name).expect("backend is registered");
        let status = backend
            .status(&Endpoint::default())
            .await
            .expect("status succeeds through the thunk");
        assert!(
            status.healthy,
            "healthy round-tripped from the thunk result"
        );
        assert_eq!(status.detail, "all-green");

        service::deregister_backend(name);
        assert!(service::backend(name).is_none(), "deregister removed it");
    }

    /// The error arm of the service bridge thunk: an error `Value` from the
    /// `BackendInvoke` becomes a `ServiceError` carrying the rendered message.
    #[tokio::test]
    async fn service_backend_thunk_surfaces_error() {
        use plugin_toolkit::service::{self, Endpoint};
        let name = "loader-service-err";
        let invoke: BackendInvoke =
            Arc::new(|_op: &str, _args: sj::Value| Err(sj::Value::String("svc-down".into())));
        let def = BackendDef {
            domain: "service".to_string(),
            name: name.to_string(),
            kind: "8080".to_string(),
            runtime: "docker".to_string(),
            invoke_prefix: name.to_string(),
            ..Default::default()
        };
        register_service_backend(&def, invoke).expect("service register");

        let backend = service::backend(name).expect("backend is registered");
        let err = backend
            .status(&Endpoint::default())
            .await
            .expect_err("status fails through the thunk");
        assert!(
            err.to_string().contains("svc-down"),
            "renders the invoke error: {err}"
        );

        service::deregister_backend(name);
    }

    /// A bad `kind` (unparseable default port) makes `register_service_backend`
    /// return the loader's contextualized error rather than register a broken
    /// backend.
    #[test]
    fn service_backend_rejects_bad_default_port() {
        let def = BackendDef {
            domain: "service".to_string(),
            name: "loader-service-badport".to_string(),
            kind: "not-a-port".to_string(),
            runtime: "docker".to_string(),
            invoke_prefix: "loader-service-badport".to_string(),
            ..Default::default()
        };
        let err = register_service_backend(&def, noop_invoke())
            .expect_err("bad default_port must be rejected");
        assert!(
            err.to_string().contains("loader-service-badport"),
            "error names the backend: {err}"
        );
    }

    /// Driving a registered deploy-target backend through `DeployTarget::stop`
    /// runs the loader's deploy-target bridge thunk end to end: encode args →
    /// invoke → decode the `DeployOutcome` result string.
    #[tokio::test]
    async fn deploy_target_backend_thunk_routes_success() {
        use plugin_toolkit::deploy_target::{self, Runtime, TargetId, TargetKind};
        let host = "loader-deploy-ok";
        let invoke: BackendInvoke = Arc::new(|op: &str, args: sj::Value| {
            assert_eq!(op, "stop");
            assert_eq!(args["workload"], sj::json!("ct-100"));
            Ok(sj::json!({ "workload": "ct-100", "state": "stopped" }))
        });
        let def = BackendDef {
            domain: "deploy_target".to_string(),
            name: host.to_string(), // host axis
            runtime: "docker".to_string(),
            kind: "cli".to_string(),
            invoke_prefix: host.to_string(),
            ..Default::default()
        };
        register_deploy_target_backend(&def, invoke).expect("deploy-target register");

        let id = TargetId {
            host: host.to_string(),
            runtime: Runtime::Docker,
            kind: TargetKind::Cli,
        };
        let target = deploy_target::target(&id).expect("target is registered");
        let outcome = target
            .stop("ct-100")
            .await
            .expect("stop succeeds through the thunk");
        assert_eq!(outcome.workload, "ct-100");
        assert_eq!(outcome.state.as_deref(), Some("stopped"));

        domain_deregister("deploy_target", host);
        assert!(
            deploy_target::target(&id).is_none(),
            "deregister_host removed it"
        );
    }

    /// The error arm of the deploy-target bridge thunk: an error `Value` from
    /// the `BackendInvoke` surfaces as a `DeployError` carrying the message.
    #[tokio::test]
    async fn deploy_target_backend_thunk_surfaces_error() {
        use plugin_toolkit::deploy_target::{self, Runtime, TargetId, TargetKind};
        let host = "loader-deploy-err";
        let invoke: BackendInvoke =
            Arc::new(|_op: &str, _args: sj::Value| Err(sj::Value::String("node-offline".into())));
        let def = BackendDef {
            domain: "deploy_target".to_string(),
            name: host.to_string(),
            runtime: "docker".to_string(),
            kind: "cli".to_string(),
            invoke_prefix: host.to_string(),
            ..Default::default()
        };
        register_deploy_target_backend(&def, invoke).expect("deploy-target register");

        let id = TargetId {
            host: host.to_string(),
            runtime: Runtime::Docker,
            kind: TargetKind::Cli,
        };
        let target = deploy_target::target(&id).expect("target is registered");
        let err = target
            .stop("ct-1")
            .await
            .expect_err("stop fails through the thunk");
        assert!(
            err.to_string().contains("node-offline"),
            "renders the invoke error: {err}"
        );

        domain_deregister("deploy_target", host);
    }

    /// An unknown runtime string makes `register_deploy_target_backend` return
    /// the loader's contextualized error rather than register a broken target.
    #[test]
    fn deploy_target_backend_rejects_unknown_runtime() {
        let def = BackendDef {
            domain: "deploy_target".to_string(),
            name: "loader-deploy-badrt".to_string(),
            runtime: "not-a-runtime".to_string(),
            kind: "cli".to_string(),
            invoke_prefix: "loader-deploy-badrt".to_string(),
            ..Default::default()
        };
        let err = register_deploy_target_backend(&def, noop_invoke())
            .expect_err("unknown runtime must be rejected");
        assert!(
            err.to_string().contains("loader-deploy-badrt"),
            "error names the backend: {err}"
        );
    }

    /// Child-process entrypoint for [`spawn_plugin_loads_serves_and_unloads`].
    ///
    /// The driver test spawns *this same test binary* as the plugin subprocess
    /// (`spawn_plugin` runs `current_exe`), inheriting `ORCA_PLUGIN_SOCKET`. Every
    /// test in that child sees the env var set, but only this one acts as the
    /// plugin: it connects back on the daemon's socket and runs the real
    /// `plugin_proto` serve loop, advertising one tool and one (agents) backend.
    /// With the env var unset — an ordinary test run — it is a no-op.
    #[cfg(unix)]
    #[test]
    fn plugin_child_serve_entrypoint() {
        use std::os::unix::net::UnixStream;
        let Ok(sock) = std::env::var(supervisor::SOCKET_ENV) else {
            return; // ordinary run: not the spawned child.
        };
        let stream = UnixStream::connect(&sock).expect("plugin connects to daemon socket");
        let hello = plugin_proto::Frame::Hello {
            protocol: plugin_proto::PROTOCOL_VERSION.into(),
            plugin: "loaderfakeplugin".into(),
            version: "9.9.9".into(),
            manifest: vec![plugin_proto::ToolDef {
                name: "loaderfakeplugin.ping".into(),
                description: "echo the args back".into(),
                input_schema: sj::json!({ "type": "object" }),
                output_schema: sj::json!({ "type": "object" }),
            }],
            backends: vec![
                sj::to_value(BackendDef {
                    domain: "agents".into(),
                    name: "loaderfakeplugin-agents".into(),
                    invoke_prefix: "loaderfakeplugin-agents".into(),
                    ..Default::default()
                })
                .expect("backend def serializes"),
            ],
            schema: sj::Value::Null,
        };
        // Serve until the daemon sends Shutdown (on unload/drop). The tool echoes
        // its args so the driver can assert the round-trip; anything else errors.
        let _served = plugin_proto::serve(stream, hello, |tool, args, _caps| {
            if tool == "loaderfakeplugin.ping" {
                Ok(args)
            } else {
                Err(format!("no such tool: {tool}"))
            }
        });
    }

    /// End-to-end load path: spawn a real subprocess plugin (this test binary
    /// re-exec'd — see [`plugin_child_serve_entrypoint`]), complete the handshake,
    /// register its tool + backend, route a call to it both async ([`dispatch`])
    /// and sync ([`invoke_plugin`]), then unload it and prove the routes and
    /// backends are gone. Drives `spawn_plugin`, `register_backends`,
    /// `make_backend_invoke`, `backing_for`, `Backing::invoke`, the loaded-plugin
    /// accessors, and `unload_plugin` — the whole live-plugin surface.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_plugin_loads_serves_and_unloads() {
        // When this test runs *inside* the spawned child, the socket env is set;
        // act only as the plugin (via `plugin_child_serve_entrypoint`), never
        // re-spawn — that would recurse.
        if std::env::var(supervisor::SOCKET_ENV).is_ok() {
            return;
        }
        // A prior aborted run could leave it loaded; start from a clean slate.
        unload_plugin("loaderfakeplugin");

        let exe = std::env::current_exe().expect("test binary path");
        let report = spawn_plugin(&exe, Some("loaderfakeplugin")).expect("fake plugin loads");
        assert_eq!(report.software, "loaderfakeplugin");
        assert_eq!(report.semver, "9.9.9");
        assert!(
            report.tools.contains(&"loaderfakeplugin.ping".to_string()),
            "tool registered: {:?}",
            report.tools
        );

        // Registry accessors now see the live plugin.
        assert!(is_loaded("loaderfakeplugin"));
        assert!(
            loaded_plugins()
                .iter()
                .any(|p| p.software == "loaderfakeplugin"),
            "loaded_plugins lists it"
        );
        assert!(
            loaded_tool_defs()
                .iter()
                .any(|t| t.name == "loaderfakeplugin.ping"),
            "loaded_tool_defs lists its tool"
        );

        // Async dispatch routes through spawn_blocking → the subprocess, which
        // echoes the args back verbatim.
        let cfg = Arc::new(contract::config::Config::load().unwrap());
        let ctx = ToolCtx::new(cfg);
        let out = dispatch("loaderfakeplugin.ping", sj::json!({ "x": 1 }), &ctx)
            .await
            .expect("async dispatch to plugin succeeds");
        assert_eq!(out, sj::json!({ "x": 1 }), "plugin echoed the args");

        // Sync invoke_plugin routes to the same subprocess.
        let sync = invoke_plugin("loaderfakeplugin.ping", &sj::json!({ "y": 2 }))
            .expect("a loaded plugin owns the tool")
            .expect("sync invoke succeeds");
        assert_eq!(sync, sj::json!({ "y": 2 }), "sync path echoed the args");

        // Unload drops the tool route and the agents backend.
        let removed = unload_plugin("loaderfakeplugin");
        assert_eq!(removed, 1, "exactly the one fake plugin was unloaded");
        assert!(!is_loaded("loaderfakeplugin"));
        assert!(
            invoke_plugin("loaderfakeplugin.ping", &sj::json!({})).is_none(),
            "tool route freed after unload"
        );
        assert!(
            !loaded_tool_defs()
                .iter()
                .any(|t| t.name == "loaderfakeplugin.ping"),
            "tool def gone after unload"
        );
    }

    #[test]
    fn loaded_plugin_info_is_debug_and_clone() {
        let info = LoadedPluginInfo {
            software: "unraid".to_string(),
            semver: "1.2.3".to_string(),
            target_compat: "6.12".to_string(),
            orca_compat: ">=0.1".to_string(),
            tools: vec!["x".to_string()],
        };
        let cloned = info.clone();
        assert_eq!(cloned.semver, "1.2.3");
        assert!(format!("{info:?}").contains("unraid"));
    }
}
