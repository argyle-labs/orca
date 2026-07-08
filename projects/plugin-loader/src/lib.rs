// The tool surface crosses this loader as opaque JSON (`sj::Value`) — the JSON
// dispatch protocol of the type-erased boundary, identical to dispatch's
// `ErasedTool::run_json`. This is the designated opaque seam; the workspace
// disallowed-types lint is suppressed for this file only.
#![allow(clippy::disallowed_types)]

//! Runtime loader for ABI-stable cdylib plugins.
//!
//! ## What this crate does
//!
//! 1. `dlopen`s a cdylib plugin via [`abi_stable`]'s [`RootModule::load_from_file`],
//!    which runs the layout+version check and returns a `PluginModRef` — or a
//!    clean `LibraryError` if the plugin's ABI is incompatible. No UB path.
//! 2. Reads the plugin's version header (`PluginMod` metadata accessors) and
//!    verifies its declared `orca_compat` range admits the running orca version.
//! 3. Calls `PluginMod::manifest` to learn the plugin's tool surface and
//!    registers each tool into a process-global runtime registry.
//! 4. Exposes [`dispatch`] — the same `(name, args, ctx) -> Result<Value>` shape
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

use abi_stable::library::{LibraryError, lib_header_from_path};
use abi_stable::std_types::{RResult, RStr, RString};
use anyhow::{Context, Result, anyhow, bail};
use contract::ToolCtx;
use plugin_toolkit::abi::{BackendDef, PluginModRef, SchemaDecl, ToolDef};
// `Value` is the JSON dispatch protocol across the type-erased tool boundary —
// the same opaque layer `dispatch::ErasedTool::run_json` uses. Aliased so the
// payload type is named once, here, at the designated opaque seam.
use serde_json as sj;

/// A single dynamically-loaded plugin, kept alive for the process lifetime.
///
/// The `PluginModRef` borrows from the `'static` library image abi_stable keeps
/// mapped after load, so it is safe to store and call indefinitely.
struct LoadedPlugin {
    /// `target_software` reported by the plugin header, e.g. `"jellyfin"`.
    software: String,
    /// The plugin's own semver.
    semver: String,
    /// Free-form target-software compat range, e.g. `"10.8-10.10"`.
    target_compat: String,
    /// The orca-version semver range the plugin declared.
    orca_compat: String,
    /// How this plugin is invoked — an in-process cdylib module, or an
    /// out-of-process subprocess over the wire protocol.
    backing: Backing,
    /// Tool defs parsed from `manifest()` at load time, keyed by tool name.
    tools: HashMap<String, ToolDef>,
    /// `(domain, backend_name)` pairs this plugin registered with domain
    /// registries (storage, …). Recorded so [`unload_plugin`] can reverse each
    /// registration — the deregistration path a reload/unload needs so a
    /// dropped cdylib doesn't leave stale backends pointing at a dead invoke
    /// thunk.
    domain_backends: Vec<(String, String)>,
}

/// How a loaded plugin's tools are invoked. Both variants expose the same
/// `(tool, args_json) -> result_json` contract, so the registry, `dispatch`,
/// and `unload` treat every plugin uniformly regardless of backing.
#[derive(Clone)]
enum Backing {
    /// In-process cdylib `dlopen`'d via `abi_stable` — the original model.
    /// `PluginModRef` is `Copy` and borrows the process-lifetime library image.
    Cdylib(PluginModRef),
    /// Out-of-process subprocess spoken to over the `plugin_proto` wire protocol
    /// ([`supervisor::PluginProcess`]). Crash-isolated; libc/ABI-independent.
    #[cfg(unix)]
    Process(Arc<supervisor::PluginProcess>),
}

impl Backing {
    /// Invoke `tool` with JSON-encoded `args_json`, returning the tool's raw
    /// result JSON or an error string. Uniform across both backings so callers
    /// never branch on how a plugin is hosted.
    fn invoke(&self, tool: &str, args_json: &str) -> std::result::Result<String, String> {
        match self {
            Backing::Cdylib(module) => {
                match (module.invoke())(RStr::from_str(tool), RStr::from_str(args_json)) {
                    RResult::ROk(out) => Ok(out.into_string()),
                    RResult::RErr(msg) => Err(msg.into_string()),
                }
            }
            #[cfg(unix)]
            Backing::Process(proc) => {
                let args: sj::Value = sj::from_str(args_json)
                    .map_err(|e| format!("encode args for '{tool}': {e}"))?;
                match proc.invoke(tool, args) {
                    Ok(value) => sj::to_string(&value)
                        .map_err(|e| format!("serialize result for '{tool}': {e}")),
                    Err(e) => Err(format!("{e:#}")),
                }
            }
        }
    }
}

/// A domain's constructor: given one backend descriptor and a thunk that calls
/// back across the plugin's FFI `invoke` boundary, register the backend with
/// that domain's process-global registry. The loader's dispatch table maps a
/// `BackendDef::domain` string to one of these so the loader stays
/// domain-agnostic — storage is the first entry; adding a domain is adding a
/// row here, not editing the load path.
type DomainRegister = fn(&BackendDef, BackendInvoke) -> Result<()>;

/// The synchronous thunk a domain proxy drives to reach the plugin: it maps an
/// `op` to a `"{invoke_prefix}.{op}"` tool call across the FFI `invoke`
/// boundary and returns the raw result/error JSON. `Send + Sync + 'static` so
/// domain proxies can offload it onto a blocking pool.
type BackendInvoke = Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync>;

/// Domain dispatch table: `domain` → constructor. Domain-agnostic loader seam.
fn domain_register(domain: &str) -> Option<DomainRegister> {
    match domain {
        "storage" => Some(register_storage_backend),
        "service" => Some(register_service_backend),
        "deploy_target" => Some(register_deploy_target_backend),
        "notifications" => Some(register_notify_backend),
        "cluster_roster" => Some(register_cluster_roster_backend),
        "topology" => Some(register_topology_backend),
        "host_facts" => Some(register_host_facts_backend),
        "service_identity" => Some(register_service_identity_backend),
        "diagnostics" => Some(register_diagnostics_backend),
        "agents" => Some(register_agent_provider_backend),
        "container_runtime" => Some(register_container_runtime_backend),
        "unit" => Some(register_unit_backend),
        "web" => Some(register_web_backend),
        _ => None,
    }
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

/// Storage-domain entry in the dispatch table: parse the descriptor's
/// kind/capabilities and register a `StorageProxy` that routes operations back
/// through `invoke`. Wraps the loader's string-error thunk into the storage
/// crate's `StorageError`-returning [`plugin_toolkit::storage::InvokeThunk`].
fn register_storage_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::storage::{self, InvokeThunk, StorageError};
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        invoke(op, args_json).map_err(StorageError::Transport)
    });
    storage::register_from_def(
        def.name.clone(),
        &def.kind,
        def.endpoint.clone(),
        &def.capabilities,
        thunk,
    )
    .map_err(|e| anyhow!("register storage backend '{}': {e}", def.name))
}

/// Service-domain entry in the dispatch table: register a `ServiceProxy` that
/// routes lifecycle ops (deploy/backup/restore/configure/status) back through
/// `invoke`. The descriptor reuses `BackendDef`'s generic axes — `kind` carries
/// the default port, `runtime` the supported-modality CSV. Wraps the loader's
/// string-error thunk into the service crate's `ServiceError`-returning
/// [`plugin_toolkit::service::InvokeThunk`].
fn register_service_backend(def: &BackendDef, invoke: BackendInvoke) -> Result<()> {
    use plugin_toolkit::service::{self, InvokeThunk, ServiceError};
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        invoke(op, args_json).map_err(ServiceError::Transport)
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
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        invoke(op, args_json).map_err(DeployError::Transport)
    });
    deploy_target::register_from_def(
        def.name.clone(), // host axis
        &def.runtime,
        &def.kind,
        def.endpoint.clone(),
        &def.capabilities,
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
    let thunk: InvokeThunk = Arc::new(move |op: &str, args_json: String| {
        invoke(op, args_json).map_err(BackendError::Transport)
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
        "service_identity" => {
            contract::service_identity::deregister_backend(name);
        }
        "diagnostics" => {
            contract::diagnostics::deregister_provider(name);
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
        other => tracing::warn!(domain = %other, %name, "deregister for unknown domain ignored"),
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
/// (cheap to clone — a `Copy` module ref or an `Arc` process handle) and its
/// `invoke_prefix`, so each proxied `op` becomes a `"{prefix}.{op}"` call routed
/// through the same backing the plugin's tools use — cdylib FFI or subprocess
/// socket, transparently.
fn make_backend_invoke(backing: Backing, invoke_prefix: String) -> BackendInvoke {
    Arc::new(move |op: &str, args_json: String| {
        let tool = format!("{invoke_prefix}.{op}");
        backing.invoke(&tool, &args_json)
    })
}

/// Register every backend a plugin declares into its domain registry, routing
/// each through `backing`. On any failure (unknown domain, constructor error)
/// the already-registered backends for this plugin are rolled back so a partial
/// load never leaves orphans. Returns the `(domain, name)` pairs registered, for
/// the caller to record on the `LoadedPlugin` (so unload can reverse them).
///
/// Shared by the cdylib ([`load_plugin`]) and subprocess ([`spawn_plugin`])
/// paths — the only difference is the [`Backing`] behind the invoke thunk.
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

/// Core's DB service, handed to every plugin via `PluginMod::set_host`. The
/// plugin sends a JSON [`DbOp`]; core runs it on its single pooled connection
/// (`exec_db_op_pooled`) and returns a JSON [`DbReply`] — so no plugin ever
/// opens a second connection to the encrypted db (the SHMOPEN 5898 race).
extern "C" fn core_db_op(op_json: RStr<'_>) -> RResult<RString, RString> {
    use plugin_toolkit::abi::{DbOp, DbReply};
    let parsed: std::result::Result<DbOp, _> = sj::from_str(op_json.as_str());
    let reply: Result<DbReply> = parsed
        .map_err(|e| anyhow!("parse DbOp: {e}"))
        .and_then(|op| db::plugin_tables::exec_db_op_pooled(&op));
    match reply.and_then(|r| sj::to_string(&r).map_err(|e| anyhow!("serialize DbReply: {e}"))) {
        Ok(s) => RResult::ROk(RString::from(s)),
        Err(e) => RResult::RErr(RString::from(format!("{e:#}"))),
    }
}

/// Core's secrets service, handed to every plugin via `PluginMod::set_secret_op`.
/// The plugin sends a JSON [`SecretOp`]; core runs it (crypto + tables) on its
/// single pooled connection (`exec_secret_op_pooled`) — so `plugin_toolkit::secrets`
/// never opens its own connection (the SHMOPEN 5898 race).
extern "C" fn core_secret_op(op_json: RStr<'_>) -> RResult<RString, RString> {
    use plugin_toolkit::abi::{SecretOp, SecretReply};
    let parsed: std::result::Result<SecretOp, _> = sj::from_str(op_json.as_str());
    let reply: Result<SecretReply> = parsed
        .map_err(|e| anyhow!("parse SecretOp: {e}"))
        .and_then(|op| db::secrets::exec_secret_op_pooled(&op));
    match reply.and_then(|r| sj::to_string(&r).map_err(|e| anyhow!("serialize SecretReply: {e}"))) {
        Ok(s) => RResult::ROk(RString::from(s)),
        Err(e) => RResult::RErr(RString::from(format!("{e:#}"))),
    }
}

/// Load a cdylib plugin from `path`, run the full compatibility gate, and
/// register its tool surface into the runtime registry.
///
/// `orca_version` is the running orca version (e.g. from `ORCA_VERSION`); it is
/// checked against the plugin's declared `orca_compat` semver range. Returns a
/// [`LoadReport`] on success, or an error describing exactly which gate failed.
pub fn load_plugin(path: &Path, orca_version: &str) -> Result<LoadReport> {
    // ── Gate 1: abi_stable layout + version check (clean refusal, never UB) ──
    //
    // Load via the per-LIBRARY `LibHeader`, NOT `PluginModRef::load_from_file`.
    // `load_from_file` caches the root module in a process-global `LateStaticRef`
    // keyed by the root-module *type* (`PluginModRef`); since every plugin shares
    // that one type, the first `load_from_file` wins and every later load of a
    // DIFFERENT cdylib returns the first plugin's module — so only one plugin
    // could ever load. `lib_header_from_path` opens this specific library and
    // `init_root_module` resolves the root module from that header's own cell
    // (still running the full version + layout gate), so each cdylib yields its
    // own module and N plugins coexist.
    let header = lib_header_from_path(path)
        .map_err(|e: LibraryError| anyhow!("ABI/layout check failed for {path:?}: {e}"))?;
    let module: PluginModRef = header
        .init_root_module::<PluginModRef>()
        .map_err(|e: LibraryError| anyhow!("ABI/layout check failed for {path:?}: {e}"))?;

    // ── Read the version header ──────────────────────────────────────────────
    let software = module.target_software()().to_string();
    let semver = module.plugin_semver()().to_string();
    let target_compat = module.target_compat()().to_string();
    let orca_compat = module.orca_compat()().to_string();

    // ── Gate 2: semantic orca-version compatibility ──────────────────────────
    let req = semver::VersionReq::parse(&orca_compat).with_context(|| {
        format!("plugin '{software}' has unparseable orca_compat '{orca_compat}'")
    })?;
    let running = semver::Version::parse(strip_pre_build(orca_version))
        .with_context(|| format!("unparseable running orca version '{orca_version}'"))?;
    if !req.matches(&running) {
        bail!(
            "plugin '{software}' v{semver} requires orca {orca_compat}, but running orca is {orca_version}"
        );
    }

    // ── Install core's DB service ────────────────────────────────────────────
    // Hand the plugin core's single pooled connection before any tool runs, so
    // every generated CRUD op routes through core instead of the plugin opening
    // its own (racing) connection. A plugin predating `set_host` gets the ABI
    // no-op default and simply keeps using its own `open_db`.
    (module.set_host())(plugin_toolkit::abi::HostDbOp { func: core_db_op });
    (module.set_secret_op())(plugin_toolkit::abi::HostSecretOp {
        func: core_secret_op,
    });

    // ── Parse the tool manifest ──────────────────────────────────────────────
    let manifest_json = module.manifest()().to_string();
    let defs: Vec<ToolDef> = sj::from_str(&manifest_json)
        .with_context(|| format!("plugin '{software}' returned an invalid manifest"))?;
    let tools: HashMap<String, ToolDef> = defs.into_iter().map(|d| (d.name.clone(), d)).collect();
    let mut tool_names: Vec<String> = tools.keys().cloned().collect();
    tool_names.sort();

    // ── Register, refusing names already known (built-in or another plugin) ──
    let mut reg = registry().write().expect("plugin registry poisoned");
    for name in &tool_names {
        if reg.by_tool.contains_key(name) {
            bail!("plugin '{software}' tool '{name}' collides with an already-loaded plugin tool");
        }
        if dispatch::tool_exists(name) {
            bail!("plugin '{software}' tool '{name}' collides with a built-in tool");
        }
    }
    // ── Parse + register domain backends (after tools, before commit) ────────
    // Forward-compatible: a plugin predating the `backends` field observes the
    // per-field default `"[]"`, so old plugins (jellyfin) register zero
    // backends and load unchanged. Parse/unknown-domain is an atomic bail —
    // anything already registered for this plugin is rolled back so a partial
    // load never leaves orphan backends.
    let backends_json = module.backends()().to_string();
    let backend_defs: Vec<BackendDef> = sj::from_str(&backends_json)
        .with_context(|| format!("plugin '{software}' returned an invalid backends list"))?;

    // ── Parse the declared SQL-table schemas ─────────────────────────────────
    // The plugin declares its config/data tables (full typed shapes, namespaced
    // to itself); the caller (installer, which owns a db connection) applies
    // them via `db::plugin_tables::apply_decl`. The loader only surfaces the
    // declaration — it does not own db lifecycle. An old plugin predating the
    // field yields an empty declaration (no namespace, no tables).
    let schemas_json = module.schemas()().to_string();
    let declared_schema: SchemaDecl = sj::from_str(&schemas_json)
        .with_context(|| format!("plugin '{software}' returned an invalid schema declaration"))?;

    let backing = Backing::Cdylib(module);
    let registered = register_backends(&backing, &software, &backend_defs)?;

    let idx = reg.plugins.len();
    for name in &tool_names {
        reg.by_tool.insert(name.clone(), idx);
    }
    let backend_names: Vec<String> = registered.iter().map(|(_, n)| n.clone()).collect();
    reg.plugins.push(LoadedPlugin {
        software: software.clone(),
        semver: semver.clone(),
        target_compat: target_compat.clone(),
        orca_compat: orca_compat.clone(),
        backing,
        tools,
        domain_backends: registered,
    });

    tracing::info!(
        plugin = %software,
        version = %semver,
        target_compat = %target_compat,
        tools = ?tool_names,
        backends = ?backend_names,
        "loaded cdylib plugin"
    );

    Ok(LoadReport {
        software,
        semver,
        tools: tool_names,
        declared_schema,
    })
}

/// Spawn an out-of-process plugin executable, complete the wire-protocol
/// handshake, and register its tool surface into the same runtime registry that
/// backs cdylib plugins — so `dispatch`/`invoke_plugin` route to it uniformly.
///
/// This is the subprocess counterpart to [`load_plugin`]. There is no
/// `orca_compat` semver gate: compatibility is negotiated at runtime as a
/// wire-protocol major match inside [`supervisor::PluginProcess::spawn`], the
/// replacement for the `abi_stable` layout tag.
///
/// Backends (topology / unit / host_facts / …) register through the same domain
/// dispatch table as the cdylib path: the plugin sends each backend def as
/// verbatim JSON over the wire ([`Frame::Hello`](plugin_proto::Frame::Hello)'s
/// `backends`), which the daemon parses into its own `abi::BackendDef` — so no
/// field is lost, and each backend's ops route back through the subprocess.
#[cfg(unix)]
pub fn spawn_plugin(exe: &Path) -> Result<LoadReport> {
    let proc = supervisor::PluginProcess::spawn(exe)?;
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
    // the wire; parse each into `abi::BackendDef` here (same type the cdylib path
    // parses from `module.backends()`).
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

    let mut reg = registry().write().expect("plugin registry poisoned");
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
    let reg = registry().read().expect("plugin registry poisoned");
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
    let reg = registry().read().expect("plugin registry poisoned");
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
    let reg = registry().read().expect("plugin registry poisoned");
    reg.plugins.iter().any(|p| p.software == software)
}

/// Unregister every loaded plugin whose `target_software` matches `software`,
/// dropping its tool-name routes so the names free up again.
///
/// Note: this removes the plugin from the *routing* registry; abi_stable keeps
/// the underlying library image mapped for the process lifetime (there is no
/// safe unmap once a `PluginModRef` has been handed out). A reinstall under the
/// same name therefore re-registers cleanly, and the orphaned image is reclaimed
/// at process exit. Returns the number of plugins removed.
pub fn unload_plugin(software: &str) -> usize {
    let mut reg = registry().write().expect("plugin registry poisoned");
    let before = reg.plugins.len();
    let removed_tools: Vec<String> = reg
        .plugins
        .iter()
        .filter(|p| p.software == software)
        .flat_map(|p| p.tools.keys().cloned())
        .collect();
    // Reverse every domain-backend registration the unloaded plugins made, so a
    // dropped cdylib leaves no storage (etc.) backend pointing at a dead invoke
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
    let reg = registry().read().expect("plugin registry poisoned");
    let idx = *reg.by_tool.get(name)?;
    let plugin = &reg.plugins[idx];
    Some((plugin.backing.clone(), plugin.software.clone()))
}

/// Marshal an invoke result JSON string into a `Value`, with a plugin-named
/// context on parse failure.
fn parse_invoke_result(
    result: std::result::Result<String, String>,
    name: &str,
    software: &str,
) -> Result<sj::Value> {
    match result {
        Ok(out) => sj::from_str(&out)
            .with_context(|| format!("plugin '{software}' returned invalid JSON for '{name}'")),
        Err(msg) => Err(anyhow!("plugin tool '{name}' failed: {msg}")),
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
        let args_json =
            sj::to_string(&args).with_context(|| format!("failed to encode args for '{name}'"))?;
        let owned = name.to_string();
        let result = tokio::task::spawn_blocking(move || backing.invoke(&owned, &args_json))
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
/// `block_on` on it). For an in-process cdylib plugin there is no such
/// constraint.
pub fn invoke_plugin(name: &str, args: &sj::Value) -> Option<Result<sj::Value>> {
    let (backing, software) = backing_for(name)?;
    let args_json = match sj::to_string(args) {
        Ok(s) => s,
        Err(e) => return Some(Err(anyhow!("failed to encode args for '{name}': {e}"))),
    };
    let result = backing.invoke(name, &args_json);
    Some(parse_invoke_result(result, name, &software))
}

/// Strip a `-pre` / `+build` suffix so a `-dev`-tagged orca build still parses
/// as a clean semver for range matching (we match on the release triple).
fn strip_pre_build(v: &str) -> &str {
    let v = v.strip_prefix('v').unwrap_or(v);
    let end = v.find(['-', '+']).unwrap_or(v.len());
    &v[..end]
}
