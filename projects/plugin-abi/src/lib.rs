//! ABI-stable cdylib plugin boundary.
//!
//! This crate is the *one* canonical contract shared by orca and every
//! external, independently-compiled plugin repo. A plugin is built as a
//! `cdylib`, exports a single [`PluginModRef`] root module via
//! [`abi_stable`]'s `#[export_root_module]`, and orca's `plugin-loader`
//! crate `dlopen`s it and runs the layout+version check before touching it.
//!
//! It is deliberately isolated from `plugin-toolkit`: the seam depends only on
//! `abi_stable` + `serde` + `schemars`, so a consumer that needs only the wire
//! contract (the loader, a thin plugin) does not pull orca core. `plugin-toolkit`
//! re-exports this crate as `plugin_toolkit::abi` for source-compatibility.
//!
//! ## Why a JSON surface instead of abi_stable-ifying every tool
//!
//! `OrcaTool` / `ToolCtx` / the schemars schema types are deep, generic, and
//! not `#[repr(C)]`. Making them all `StableAbi` would be a massive, fragile
//! surface. Instead only the *entrypoint + metadata* cross the FFI boundary
//! as `StableAbi` types, and the tool surface itself crosses as JSON — the
//! same representation dispatch already normalizes to (`ErasedTool::run_json`
//! / `input_schema()`).
//!
//! - [`PluginMod::manifest`] returns a JSON array of [`ToolDef`] — the same
//!   shape `dispatch`/`openapi` already expect.
//! - [`PluginMod::invoke`] takes a tool name + a JSON args blob and returns
//!   a JSON result (or a JSON-string error). The plugin owns whatever async
//!   runtime it needs internally; the FFI call is synchronous.
//!
//! ## The compatibility gate
//!
//! Two independent layers gate compatibility, and both refuse cleanly
//! (returning an `Err`, never undefined behaviour):
//!
//! 1. **Layout/version (abi_stable):** `RootModule::load_from_file` /
//!    `lib_header_from_path` verify the `abi_stable` layout hash and the
//!    crate version baked into the header. A plugin built against an
//!    incompatible toolkit ABI is rejected here.
//! 2. **Semantic compat (this crate):** the [`PluginMod`] header carries
//!    the plugin's own semver plus the target-software name + compat range
//!    and the orca-version range it supports. The loader reads these and
//!    refuses a plugin whose declared orca-compat range does not admit the
//!    running orca version.

use abi_stable::StableAbi;
use abi_stable::library::RootModule;
use abi_stable::package_version_strings;
use abi_stable::sabi_types::VersionStrings;
use abi_stable::std_types::{RResult, RStr, RString};
use schemars::Schema;

/// The ABI-stable root module every orca plugin cdylib exports.
///
/// Field order + types are layout-hashed by `abi_stable`; changing them is
/// an ABI break that the load-time check will catch. The header fields are
/// data the loader reads *before* invoking anything, so a refusal costs
/// nothing.
//
// All fields are `extern "C"` function pointers (which are `Copy`): an
// abi_stable `Prefix` RootModule generates by-value accessors, so storing
// non-`Copy` data (e.g. `RString`) directly as a field would not compile.
// The version/metadata strings are therefore exposed as zero-argument
// `fn() -> RString` accessors rather than bare fields.
#[repr(C)]
#[derive(StableAbi)]
#[sabi(kind(Prefix(prefix_ref = PluginModRef)))]
#[sabi(missing_field(panic))]
pub struct PluginMod {
    /// The plugin's own semantic version, e.g. `"0.1.0"`. Distinct from the
    /// toolkit ABI version (that lives in the abi_stable library header).
    pub plugin_semver: extern "C" fn() -> RString,

    /// External target-software identity, e.g. `"jellyfin"`. Lets the loader
    /// and operators reason about *what* the plugin integrates.
    pub target_software: extern "C" fn() -> RString,

    /// Compatibility range of the target software, e.g. `"10.8-10.10"`.
    /// Free-form for now; the loader logs it and surfaces it in diagnostics.
    pub target_compat: extern "C" fn() -> RString,

    /// The orca version range this plugin supports, e.g. `">=0.0.8, <0.1.0"`
    /// (semver `VersionReq` syntax). The loader parses this and refuses to
    /// register the plugin if the running orca version is not admitted.
    pub orca_compat: extern "C" fn() -> RString,

    /// Return a JSON array of [`ToolDef`]. Mirrors what dispatch's registry
    /// exposes for MCP/OpenAPI.
    pub manifest: extern "C" fn() -> RString,

    /// Invoke a tool by name with a JSON-encoded args object. Returns the
    /// tool's JSON-encoded output on success, or a human-readable error
    /// string on failure. The plugin drives any async work internally.
    //
    // `last_prefix_field` stays here: every field at or before the last-prefix
    // field is part of the *guaranteed* prefix (always present), and abi_stable
    // ignores `missing_field` on such fields. Fields added *after* this one are
    // the genuinely-optional, defaultable tail — which is exactly where
    // `backends` lives so an older plugin that predates it loads cleanly.
    #[sabi(last_prefix_field)]
    pub invoke: extern "C" fn(name: RStr<'_>, args_json: RStr<'_>) -> RResult<RString, RString>,

    /// Return a JSON array of [`BackendDef`] — the domain backends this plugin
    /// contributes (storage providers, etc). The loader registers each against
    /// its domain registry and routes the backend's operations back through
    /// [`PluginMod::invoke`] as a JSON proxy.
    ///
    /// Forward-compatibility: this field sits *after* the `last_prefix_field`
    /// (`invoke`), so it is part of abi_stable's optional tail. A plugin built
    /// against an older toolkit that predates this field simply doesn't export
    /// it; the per-field [`missing_field(with)`] default makes the loader
    /// observe an empty array (`"[]"`) for such plugins, so "didn't export" is
    /// identical to "exported empty" — no presence guard, no ABI break for old
    /// plugins (e.g. jellyfin built against an earlier rc).
    #[sabi(missing_field(with = default_backends))]
    pub backends: extern "C" fn() -> RString,
}

/// Default accessor for [`PluginMod::backends`] when a plugin predates the
/// field: yields a function returning an empty JSON array. The accessor's
/// return type is the field type itself (an `extern "C" fn() -> RString`), so
/// this returns *that function*, not a string. abi_stable's generated
/// `backends()` getter calls this when an older plugin's prefix ends before the
/// `backends` field, yielding a function that returns an empty JSON array.
fn default_backends() -> extern "C" fn() -> RString {
    extern "C" fn empty() -> RString {
        RString::from("[]")
    }
    empty
}

impl RootModule for PluginModRef {
    abi_stable::declare_root_module_statics! {PluginModRef}

    /// The base name of the dynamic library, sans platform prefix/suffix.
    /// Plugins built as `cdylib` produce `liborca_plugin.<ext>` so the loader
    /// resolves them by a stable, plugin-agnostic name.
    const BASE_NAME: &'static str = "orca_plugin";

    /// Human-facing name used in abi_stable's error messages.
    const NAME: &'static str = "orca_plugin";

    /// The toolkit version baked into the library header. abi_stable compares
    /// this (major/minor) at load time as part of the compatibility gate.
    const VERSION_STRINGS: VersionStrings = package_version_strings!();
}

/// JSON shape of a single tool definition in [`PluginMod::manifest`] output.
///
/// Defined here (not just documented) so plugin authors build their manifest
/// against a typed struct and the loader deserializes against the same one —
/// one canonical contract, no drift. The schema fields are `schemars::Schema`
/// (a typed, serde-(de)serializable JSON-Schema document) — the same type
/// `schemars::schema_for!` produces. This type lives *inside* the JSON blob;
/// it does not cross the FFI boundary as a type.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct ToolDef {
    /// Fully-qualified tool name, e.g. `"jellyfin.server_info"`.
    pub name: String,
    /// One-line human description.
    pub description: String,
    /// JSON Schema for the tool's args (schemars Draft 2020-12 shape).
    pub input_schema: Schema,
    /// JSON Schema for the tool's output.
    pub output_schema: Schema,
}

/// JSON shape of a single domain backend a plugin contributes, returned in
/// [`PluginMod::backends`]'s array. The loader's domain dispatch table maps
/// [`BackendDef::domain`] to a `register_from_def` constructor that builds a
/// JSON-proxy backend; the proxy routes each operation back across the FFI
/// boundary through [`PluginMod::invoke`] under [`BackendDef::invoke_prefix`].
///
/// This type lives *inside* the JSON blob; it does not cross the FFI boundary
/// as a type — one canonical contract, deserialized identically on both sides.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct BackendDef {
    /// Domain registry this backend belongs to, e.g. `"storage"`. The loader
    /// refuses a `BackendDef` whose domain has no registered constructor.
    pub domain: String,
    /// Unique backend name within its domain (e.g. `"nfs"`, `"smb"`). Used as
    /// the registry key; re-registering the same name replaces in place.
    pub name: String,
    /// Coarse kind string, domain-interpreted (storage: `network_share` /
    /// `disk_storage` / `object`). Deserialized into the domain's own enum by
    /// the domain constructor.
    pub kind: String,
    /// Non-secret endpoint string for display, e.g. `nfs://10.0.0.5:/export`.
    pub endpoint: String,
    /// Capability strings this backend advertises, domain-interpreted (storage:
    /// `list` / `mount` / `unmount` / `usage` / `recover_stale` / …).
    pub capabilities: Vec<String>,
    /// Tool-name prefix the proxy uses when calling back through `invoke`. The
    /// proxy invokes `"{invoke_prefix}.{op}"` (e.g. `"nfs.recover_stale"`) with
    /// the operation's JSON args. Lets one plugin host several backends that
    /// each map to a distinct tool family.
    pub invoke_prefix: String,
}
