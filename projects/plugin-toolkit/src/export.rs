//! cdylib export boilerplate, pulled up from each plugin's hand-written
//! `abi_export.rs`.
//!
//! **Composition, not inheritance.** A plugin opts into exactly one export
//! macro — [`export_storage_plugin!`](crate::export_storage_plugin) for a
//! backend, [`export_tool_plugin!`](crate::export_tool_plugin) for a tool
//! surface — plus these free helpers. It never extends a framework base type.
//! The macros emit only the 8 `extern "C"` ABI fns + the `#[export_root_module]`
//! wiring; everything with real logic lives here as an ordinary, unit-tested
//! function so the macros stay a thin, correct-by-construction wiring layer.
//!
//! The single unavoidable direct dep a plugin keeps is `abi_stable` itself:
//! `#[export_root_module]` expands to bare `::abi_stable` paths, so the macro
//! routes everything else through `::plugin_toolkit::*` but leaves that one.
//!
//! `clippy::disallowed_types` is allowed here for the same reason it is in each
//! plugin's `abi_export.rs`: this is the designated JSON dispatch seam, the one
//! place opaque `serde_json` payloads legitimately cross the FFI boundary.
#![allow(clippy::disallowed_types)]

use abi_stable::std_types::{RErr, ROk, RResult, RString};
use serde::Serialize;
// Aliased at this one seam exactly as `plugin-loader` and each plugin's
// `abi_export.rs` alias it, so the JSON payload type stays local to the seam.
use serde_json as sj;
use tokio::runtime::{Builder, Runtime};

/// orca-ABI compat range every plugin of this toolkit generation advertises.
/// One constant so an ABI bump is a toolkit edit, not a fleet-wide sweep.
pub const ORCA_COMPAT: &str = ">=0.0.8, <0.2.0";

/// `backends()` payload for a plugin contributing no domain backend (a pure
/// tool-surface plugin): the empty array the loader also synthesizes.
pub const EMPTY_BACKENDS: &str = "[]";

/// `schemas()` payload for a plugin declaring no plugin-scoped SQL tables: the
/// empty declaration the loader synthesizes for a plugin predating the field.
pub const EMPTY_SCHEMAS: &str = r#"{"namespace":"","tables":[]}"#;

/// Process-wide multi-thread tokio runtime that drives a plugin's async backend
/// behind the synchronous FFI `invoke`. Built once, kept for the process
/// lifetime. The FFI boundary is synchronous, so `invoke` is never already
/// inside an async context — `block_on` on this runtime is safe.
pub fn runtime() -> &'static Runtime {
    static RT: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build plugin tokio runtime")
    })
}

/// Encode a serializable op result back across the FFI boundary as `ROk(json)`.
pub fn ok_json<T: Serialize>(value: &T) -> RResult<RString, RString> {
    match sj::to_string(value) {
        Ok(s) => ROk(RString::from(s)),
        Err(e) => RErr(RString::from(format!("failed to encode result: {e}"))),
    }
}

/// Wrap a domain dispatcher's `Result<json, error-message>` as the FFI
/// `RResult` every plugin `invoke` returns. The shared tail of dispatch.
pub fn encode(result: Result<String, String>) -> RResult<RString, RString> {
    match result {
        Ok(s) => ROk(RString::from(s)),
        Err(e) => RErr(RString::from(e)),
    }
}

/// Build an `RErr` from any displayable message.
pub fn err<S: std::fmt::Display>(msg: S) -> RResult<RString, RString> {
    RErr(RString::from(msg.to_string()))
}

// ── Storage backend export glue ─────────────────────────────────────────────

/// Derive a [`BackendDef`](crate::abi::BackendDef) from a live storage backend.
///
/// The descriptor orca's loader registers is *exactly* the backend's own
/// [`provider`](crate::storage::StorageBackend::provider) — kind, endpoint and
/// capabilities all come from the trait, so a backend plugin never restates
/// them in a hand-written literal that can drift. `..Default::default()` keeps
/// the literal forward-compatible with new `BackendDef` axes (e.g. the
/// deploy-target `runtime` field).
pub fn storage_backend_def(
    backend: &dyn crate::storage::StorageBackend,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    use crate::storage::{Capability, StorageKind};

    let kind = match backend.kind() {
        StorageKind::NetworkShare => "network_share",
        StorageKind::DiskStorage => "disk_storage",
        StorageKind::Object => "object",
    };
    let capabilities = backend
        .capabilities()
        .into_iter()
        .map(|c| {
            match c {
                Capability::List => "list",
                Capability::Mount => "mount",
                Capability::Unmount => "unmount",
                Capability::Usage => "usage",
                Capability::Create => "create",
                Capability::Remove => "remove",
                Capability::RecoverStale => "recover_stale",
            }
            .to_string()
        })
        .collect();

    crate::abi::BackendDef {
        domain: "storage".to_string(),
        name: backend.name().to_string(),
        kind: kind.to_string(),
        endpoint: backend.endpoint(),
        capabilities,
        invoke_prefix: invoke_prefix.to_string(),
        ..Default::default()
    }
}

/// Serialize a one-backend `backends()` payload from a live storage backend.
pub fn storage_backends_json(
    backend: &dyn crate::storage::StorageBackend,
    invoke_prefix: &str,
) -> String {
    let def = storage_backend_def(backend, invoke_prefix);
    sj::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Route a proxied storage `op` to a backend on the shared runtime and wrap the
/// result for FFI. Thin adapter over [`storage::dispatch_op`](crate::storage::dispatch_op):
/// the storage domain owns the op set + wire-arg contract, this just bridges it
/// to the synchronous boundary.
pub fn dispatch_storage(
    backend: &dyn crate::storage::StorageBackend,
    op: &str,
    args_json: &str,
) -> RResult<RString, RString> {
    encode(runtime().block_on(crate::storage::dispatch_op(backend, op, args_json)))
}

// ── Service backend export glue ─────────────────────────────────────────────

/// Derive a [`BackendDef`](crate::abi::BackendDef) from a live service backend.
///
/// The descriptor orca registers is exactly the backend's own
/// [`descriptor`](crate::service::ServiceBackend::descriptor) — modalities,
/// port, endpoint and capabilities all come from the trait, never restated in a
/// drift-prone literal. The service domain reuses `BackendDef`'s generic axes:
/// `kind` carries the default port, `runtime` the supported-modality CSV.
pub fn service_backend_def(
    backend: &dyn crate::service::ServiceBackend,
    invoke_prefix: &str,
) -> crate::abi::BackendDef {
    let runtimes = backend
        .runtimes()
        .into_iter()
        .map(crate::service::runtime_str)
        .collect::<Vec<_>>()
        .join(",");
    let capabilities = backend
        .capabilities()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect();

    crate::abi::BackendDef {
        domain: "service".to_string(),
        name: backend.provider().to_string(),
        kind: backend.default_port().to_string(),
        runtime: runtimes,
        endpoint: backend.endpoint(),
        capabilities,
        invoke_prefix: invoke_prefix.to_string(),
    }
}

/// Serialize a one-backend `backends()` payload from a live service backend.
pub fn service_backends_json(
    backend: &dyn crate::service::ServiceBackend,
    invoke_prefix: &str,
) -> String {
    let def = service_backend_def(backend, invoke_prefix);
    sj::to_string(&[def]).unwrap_or_else(|_| "[]".to_string())
}

/// Route a proxied service `op` to a backend on the shared runtime and wrap the
/// result for FFI. Thin adapter over [`service::dispatch_op`](crate::service::dispatch_op).
pub fn dispatch_service(
    backend: &dyn crate::service::ServiceBackend,
    op: &str,
    args_json: &str,
) -> RResult<RString, RString> {
    encode(runtime().block_on(crate::service::dispatch_op(backend, op, args_json)))
}

// ── Unit backend export glue ────────────────────────────────────────────────

// The pure `unit`-backend descriptor builders (`unit_backend_def` /
// `unit_backends_json`) moved to [`backend_def`](crate::backend_def) so a thin
// subprocess plugin can advertise a unit backend without the reactor; re-exported
// so the export macros keep resolving `$crate::export::unit_backend_def` etc.
pub use crate::backend_def::{unit_backend_def, unit_backends_json};

/// Route a proxied unit `op` to a provider on the shared runtime, returning the
/// plain `Result<String, String>` a **hybrid** plugin's `backend_dispatch`
/// expects. This is the boilerplate every unit-hosting plugin's
/// `registration.rs` hand-rolls (`runtime().block_on(dispatch_op(..))`); call
/// it after stripping the invoke-prefix from the op name.
pub fn dispatch_unit_op(
    provider: &dyn crate::contract::unit::UnitProvider,
    op: &str,
    args_json: &str,
) -> Result<String, String> {
    runtime().block_on(crate::contract::unit::dispatch_op(provider, op, args_json))
}

/// FFI-wrapped variant of [`dispatch_unit_op`] for a pure unit-surface export.
pub fn dispatch_unit(
    provider: &dyn crate::contract::unit::UnitProvider,
    op: &str,
    args_json: &str,
) -> RResult<RString, RString> {
    encode(dispatch_unit_op(provider, op, args_json))
}

// ── Topology / host-facts / service-identity backend export glue ────────────

// These pure descriptor builders moved to [`backend_def`](crate::backend_def)
// so a thin subprocess plugin can advertise these backends without the reactor;
// re-exported so the export macros keep resolving `$crate::export::*` unchanged.
pub use crate::backend_def::{
    host_facts_backend_def, service_identity_backend_def, service_identity_backends_json,
    topology_backend_def, topology_backends_json,
};

// ── Tool-surface export glue (needs the dispatch registry) ──────────────────

#[cfg(feature = "tools")]
mod tool_support {
    use super::{RResult, RString, err, ok_json, runtime, sj};
    // Reactor-free manifest/ctx helpers live in `tool_manifest` (gated on
    // `tools` alone) so the thin `serve` loop can share them; re-exported below.
    pub use crate::tool_manifest::{manifest_for, manifest_for_prefixes, minimal_ctx};

    /// Decode args, run the named tool on the shared runtime against a
    /// [`minimal_ctx`], and wrap the result for FFI. Rejects names outside the
    /// plugin's `prefix` (trailing dot included).
    pub fn dispatch_tool(prefix: &str, name: &str, args_json: &str) -> RResult<RString, RString> {
        dispatch_tool_multi(&[prefix], name, args_json)
    }

    /// [`dispatch_tool`] admitting ANY of `prefixes` — the multi-app plugin's
    /// invoke. Dispatch routes by the full tool name, so a single registry call
    /// serves every hosted app; this only widens the admission check.
    pub fn dispatch_tool_multi(
        prefixes: &[&str],
        name: &str,
        args_json: &str,
    ) -> RResult<RString, RString> {
        if !prefixes.iter().any(|p| name.starts_with(p)) {
            return err(format!(
                "tool '{name}' is not in this plugin's namespace {prefixes:?}"
            ));
        }
        let args: sj::Value = match sj::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return err(format!("invalid args JSON: {e}")),
        };
        let ctx = minimal_ctx();
        match runtime().block_on(crate::dispatch::dispatch(name, args, &ctx)) {
            Ok(value) => ok_json(&value),
            Err(e) => err(format!("{e:#}")),
        }
    }
}

#[cfg(feature = "tools")]
pub use tool_support::{
    dispatch_tool, dispatch_tool_multi, manifest_for, manifest_for_prefixes, minimal_ctx,
};

// ── Export macros ───────────────────────────────────────────────────────────

/// Emit the four metadata ABI fns + the `#[export_root_module]` entrypoint that
/// are byte-identical across every plugin. The caller emits the four
/// surface-specific fns `__manifest` / `__invoke` / `__backends` / `__schemas`,
/// which the generated `PluginMod` literal references by fixed name.
///
/// Internal — invoked only by [`export_tool_plugin!`](crate::export_tool_plugin)
/// and [`export_storage_plugin!`](crate::export_storage_plugin).
#[doc(hidden)]
#[macro_export]
macro_rules! __orca_plugin_root {
    ($software:expr, $target_compat:expr $(,)?) => {
        extern "C" fn __plugin_semver() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from(::core::env!("CARGO_PKG_VERSION"))
        }
        extern "C" fn __target_software() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($software)
        }
        extern "C" fn __target_compat() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($target_compat)
        }
        extern "C" fn __orca_compat() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::ORCA_COMPAT)
        }

        // Store core's DB service so every generated CRUD op runs on core's
        // single pooled connection instead of the plugin opening its own.
        // Byte-identical across every plugin, hence emitted here.
        extern "C" fn __set_host(db_op: $crate::abi::HostDbOp) {
            $crate::runtime::set_host_db(db_op);
        }

        // Store core's secrets service (same rationale as __set_host).
        extern "C" fn __set_secret_op(secret_op: $crate::abi::HostSecretOp) {
            $crate::runtime::set_host_secret_op(secret_op);
        }

        #[$crate::abi_stable::export_root_module]
        fn __orca_export() -> $crate::abi::PluginModRef {
            use $crate::abi_stable::prefix_type::PrefixTypeTrait;
            $crate::abi::PluginMod {
                plugin_semver: __plugin_semver,
                target_software: __target_software,
                target_compat: __target_compat,
                orca_compat: __orca_compat,
                manifest: __manifest,
                invoke: __invoke,
                backends: __backends,
                schemas: __schemas,
                set_host: __set_host,
                set_secret_op: __set_secret_op,
            }
            .leak_into_prefix()
        }
    };
}

/// Export a **tool-surface** plugin's cdylib root module in one line.
///
/// Three shapes, by composition — a plugin uses exactly the arm it needs:
///
/// 1. **Pure tool** — `{ name, target_compat }`. Manifest is the plugin's own
///    `"{name}."` slice of the linked inventory; `invoke` routes through the
///    dispatch registry; `backends`/`schemas` empty.
/// 2. **Multi-app** — `{ name, target_compat, tool_prefixes: ["a.", "b."] }`.
///    One cdylib hosting several mesh namespaces (e.g. `arr` →
///    `sonarr.`/`radarr.`/`prowlarr.`/`lidarr.`). Manifest + invoke admit any
///    listed prefix; dispatch still routes by full tool name.
/// 3. **Hybrid** — `{ name, target_compat, backends, backend_dispatch }`. A tool
///    plugin that ALSO registers a domain backend (e.g. `ntfy`'s notification
///    backends). `backends` is an expression yielding the backends JSON;
///    `backend_dispatch` is a `fn(&str, &str) -> Option<Result<String, String>>`
///    that handles the domain's `*.__backend.*` calls and returns `None` to fall
///    through to tool dispatch. The plugin keeps only its genuinely-unique
///    backend logic; all the rest is generated.
///
/// ```rust,ignore
/// plugin_toolkit::export_tool_plugin! { name: "docker", target_compat: ">=20.10" }
/// plugin_toolkit::export_tool_plugin! {
///     name: "arr", target_compat: "v3",
///     tool_prefixes: ["sonarr.", "radarr.", "prowlarr.", "lidarr."],
/// }
/// plugin_toolkit::export_tool_plugin! {
///     name: "ntfy", target_compat: "",
///     backends: ntfy_backends_json(),
///     backend_dispatch: ntfy_backend_dispatch,
/// }
/// ```
#[macro_export]
macro_rules! export_tool_plugin {
    // 1. Pure single-prefix tool.
    (
        name: $name:literal,
        target_compat: $target_compat:literal $(,)?
    ) => {
        const _ORCA_TOOL_PREFIX: &str = ::core::concat!($name, ".");

        extern "C" fn __manifest() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::manifest_for(
                _ORCA_TOOL_PREFIX,
            ))
        }
        extern "C" fn __backends() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_BACKENDS)
        }
        extern "C" fn __schemas() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_SCHEMAS)
        }
        extern "C" fn __invoke(
            name: $crate::abi_stable::std_types::RStr<'_>,
            args_json: $crate::abi_stable::std_types::RStr<'_>,
        ) -> $crate::abi_stable::std_types::RResult<
            $crate::abi_stable::std_types::RString,
            $crate::abi_stable::std_types::RString,
        > {
            $crate::export::dispatch_tool(_ORCA_TOOL_PREFIX, name.as_str(), args_json.as_str())
        }

        $crate::__orca_plugin_root!($name, $target_compat);
    };

    // 2. Multi-app: one cdylib hosting several mesh namespaces (arr).
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        tool_prefixes: [ $($prefix:literal),+ $(,)? ] $(,)?
    ) => {
        extern "C" fn __manifest() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::manifest_for_prefixes(
                &[ $($prefix),+ ],
            ))
        }
        extern "C" fn __backends() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_BACKENDS)
        }
        extern "C" fn __schemas() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_SCHEMAS)
        }
        extern "C" fn __invoke(
            name: $crate::abi_stable::std_types::RStr<'_>,
            args_json: $crate::abi_stable::std_types::RStr<'_>,
        ) -> $crate::abi_stable::std_types::RResult<
            $crate::abi_stable::std_types::RString,
            $crate::abi_stable::std_types::RString,
        > {
            $crate::export::dispatch_tool_multi(
                &[ $($prefix),+ ],
                name.as_str(),
                args_json.as_str(),
            )
        }

        $crate::__orca_plugin_root!($name, $target_compat);
    };

    // 3. Hybrid: tool surface + a registered domain backend (ntfy / proxmox).
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backends: $backends:expr,
        backend_dispatch: $backend_dispatch:expr $(,)?
    ) => {
        const _ORCA_TOOL_PREFIX: &str = ::core::concat!($name, ".");

        extern "C" fn __manifest() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::manifest_for(
                _ORCA_TOOL_PREFIX,
            ))
        }
        extern "C" fn __schemas() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_SCHEMAS)
        }
        extern "C" fn __backends() -> $crate::abi_stable::std_types::RString {
            let json: ::std::string::String = $backends;
            $crate::abi_stable::std_types::RString::from(json)
        }
        extern "C" fn __invoke(
            name: $crate::abi_stable::std_types::RStr<'_>,
            args_json: $crate::abi_stable::std_types::RStr<'_>,
        ) -> $crate::abi_stable::std_types::RResult<
            $crate::abi_stable::std_types::RString,
            $crate::abi_stable::std_types::RString,
        > {
            // Domain-backend calls (`*.__backend.*`) first; the plugin's hook
            // returns Some(result) when it owns the name, else None to fall
            // through to the tool surface.
            let backend_dispatch = $backend_dispatch;
            if let ::core::option::Option::Some(res) =
                backend_dispatch(name.as_str(), args_json.as_str())
            {
                return $crate::export::encode(res);
            }
            $crate::export::dispatch_tool(_ORCA_TOOL_PREFIX, name.as_str(), args_json.as_str())
        }

        $crate::__orca_plugin_root!($name, $target_compat);
    };
}

/// Export a **storage-backend** plugin's cdylib root module in one line.
///
/// `backend` is an expression yielding a fresh backend instance (it implements
/// [`storage::StorageBackend`](crate::storage::StorageBackend)). `backends()`
/// is derived from the instance's own `provider()` — kind, endpoint and
/// capabilities are never restated — and `invoke()` routes the storage domain's
/// proxied ops through [`storage::dispatch_op`](crate::storage::dispatch_op).
/// The plugin carries no `#[orca_tool]`s, so `manifest()` is empty.
///
/// ```rust,ignore
/// plugin_toolkit::export_storage_plugin! {
///     name: "smb",
///     target_compat: "any",
///     backend: SmbBackend::new("smb"),
/// }
/// ```
#[macro_export]
macro_rules! export_storage_plugin {
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        const _ORCA_INVOKE_PREFIX: &str = ::core::concat!("storage.__backend.", $name);

        extern "C" fn __manifest() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from("[]")
        }
        extern "C" fn __schemas() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_SCHEMAS)
        }
        extern "C" fn __backends() -> $crate::abi_stable::std_types::RString {
            let backend = $backend;
            $crate::abi_stable::std_types::RString::from($crate::export::storage_backends_json(
                &backend,
                _ORCA_INVOKE_PREFIX,
            ))
        }
        extern "C" fn __invoke(
            name: $crate::abi_stable::std_types::RStr<'_>,
            args_json: $crate::abi_stable::std_types::RStr<'_>,
        ) -> $crate::abi_stable::std_types::RResult<
            $crate::abi_stable::std_types::RString,
            $crate::abi_stable::std_types::RString,
        > {
            let ::core::option::Option::Some(op) = name
                .as_str()
                .strip_prefix(_ORCA_INVOKE_PREFIX)
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                return $crate::export::err(::std::format!(
                    "tool '{}' is not in this plugin's '{}.*' namespace",
                    name.as_str(),
                    _ORCA_INVOKE_PREFIX,
                ));
            };
            let backend = $backend;
            $crate::export::dispatch_storage(&backend, op, args_json.as_str())
        }

        $crate::__orca_plugin_root!($name, $target_compat);
    };
}

/// Export a **service-backend** plugin's cdylib root module in one line.
///
/// `backend` is an expression yielding a fresh backend instance implementing
/// [`service::ServiceBackend`](crate::service::ServiceBackend). `backends()` is
/// derived from the instance's own `descriptor()` — modalities, port, endpoint
/// and capabilities are never restated — and `invoke()` routes the service
/// domain's proxied ops (`deploy`/`backup`/`restore`/`configure`/`status`)
/// through [`service::dispatch_op`](crate::service::dispatch_op). The plugin
/// carries no `#[orca_tool]`s, so `manifest()` is empty — the entire fleet
/// shares the generic `service.*` surface.
///
/// ```rust,ignore
/// plugin_toolkit::export_service_plugin! {
///     name: "audiobookshelf",
///     target_compat: "any",
///     backend: AudiobookshelfBackend::new("audiobookshelf"),
/// }
/// ```
#[macro_export]
macro_rules! export_service_plugin {
    (
        name: $name:literal,
        target_compat: $target_compat:literal,
        backend: $backend:expr $(,)?
    ) => {
        const _ORCA_INVOKE_PREFIX: &str = ::core::concat!("service.__backend.", $name);

        extern "C" fn __manifest() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from("[]")
        }
        extern "C" fn __schemas() -> $crate::abi_stable::std_types::RString {
            $crate::abi_stable::std_types::RString::from($crate::export::EMPTY_SCHEMAS)
        }
        extern "C" fn __backends() -> $crate::abi_stable::std_types::RString {
            let backend = $backend;
            $crate::abi_stable::std_types::RString::from($crate::export::service_backends_json(
                &backend,
                _ORCA_INVOKE_PREFIX,
            ))
        }
        extern "C" fn __invoke(
            name: $crate::abi_stable::std_types::RStr<'_>,
            args_json: $crate::abi_stable::std_types::RStr<'_>,
        ) -> $crate::abi_stable::std_types::RResult<
            $crate::abi_stable::std_types::RString,
            $crate::abi_stable::std_types::RString,
        > {
            let ::core::option::Option::Some(op) = name
                .as_str()
                .strip_prefix(_ORCA_INVOKE_PREFIX)
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                return $crate::export::err(::std::format!(
                    "tool '{}' is not in this plugin's '{}.*' namespace",
                    name.as_str(),
                    _ORCA_INVOKE_PREFIX,
                ));
            };
            let backend = $backend;
            $crate::export::dispatch_service(&backend, op, args_json.as_str())
        }

        $crate::__orca_plugin_root!($name, $target_compat);
    };
}

#[cfg(test)]
mod unit_backend_tests {
    use super::*;
    use crate::contract::BoxFuture;
    use crate::contract::unit::{
        KindDeclaration, UnitDescriptor, UnitProvider, VerbArgs, VerbDecl, VerbOutcome,
    };

    struct DemoProvider;

    impl UnitProvider for DemoProvider {
        fn name(&self) -> &str {
            "demo"
        }
        fn declarations(&self) -> Vec<KindDeclaration> {
            vec![
                KindDeclaration {
                    kind: "stack".into(),
                    verbs: vec![VerbDecl::list(), VerbDecl::detail()],
                },
                // Second kind repeats `list` — the capability CSV must dedup it.
                KindDeclaration {
                    kind: "container".into(),
                    verbs: vec![VerbDecl::list()],
                },
            ]
        }
        fn units(&self) -> BoxFuture<'_, crate::anyhow::Result<Vec<UnitDescriptor>>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn invoke(&self, _args: VerbArgs) -> BoxFuture<'_, crate::anyhow::Result<VerbOutcome>> {
            Box::pin(async { unreachable!("not exercised by this test") })
        }
    }

    #[test]
    fn unit_backend_def_is_derived_from_the_provider() {
        let def = unit_backend_def(&DemoProvider, "demo.__unit");
        assert_eq!(def.domain, "unit");
        assert_eq!(def.name, "demo");
        assert_eq!(def.invoke_prefix, "demo.__unit");
        // Declared kinds ride the runtime axis, in declaration order.
        assert_eq!(def.runtime, "stack,container");
        // Verbs are the deduped, sorted union across kinds.
        assert_eq!(def.capabilities, vec!["detail", "list"]);
    }

    #[test]
    fn unit_backends_json_wraps_the_def_in_a_one_element_array() {
        let json = unit_backends_json(&DemoProvider, "demo.__unit");
        assert!(json.starts_with('['));
        assert!(json.contains("\"domain\":\"unit\""));
        assert!(json.contains("\"name\":\"demo\""));
    }

    #[test]
    fn topology_backend_def_advertises_the_collect_op() {
        let def = topology_backend_def("demo", "demo");
        assert_eq!(def.domain, "topology");
        assert_eq!(def.name, "demo");
        assert_eq!(def.invoke_prefix, "demo");
        assert_eq!(
            def.capabilities,
            vec![crate::contract::topology::COLLECT_OP.to_string()]
        );
    }
}
