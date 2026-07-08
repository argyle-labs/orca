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

    /// Return a JSON [`SchemaDecl`] — the plugin's declared config/data tables
    /// (full typed REAL SQL table shapes, namespaced to the plugin). orca diffs
    /// these against what exists and applies a safe additive migration on load,
    /// into the plugin's isolated namespace (`plug__<namespace>__<table>`). The
    /// plugin declares; orca owns the db and performs every operation.
    ///
    /// Same forward-compat story as `backends`: it sits in the optional tail, so
    /// a plugin built against an older toolkit that predates it loads cleanly,
    /// observed as an empty declaration via [`default_schemas`].
    #[sabi(missing_field(with = default_schemas))]
    pub schemas: extern "C" fn() -> RString,

    /// Hand the plugin core's DB service. The loader calls this exactly once,
    /// right after the compat gate passes and before any tool runs, passing a
    /// [`HostDbOp`] bound to core's single serialized connection. The plugin
    /// stores it and routes every generated CRUD op through it — plugins never
    /// open their own connection.
    ///
    /// Optional tail field (after `invoke`, like `backends`/`schemas`): a plugin
    /// built against an older toolkit that predates it simply doesn't export it,
    /// and the [`default_set_host`] no-op makes the loader's call harmless for
    /// such plugins (they still use their own `open_db`).
    #[sabi(missing_field(with = default_set_host))]
    pub set_host: extern "C" fn(db_op: HostDbOp),

    /// Hand the plugin core's **secrets** service, bound to core's single pooled
    /// connection. Same rationale as [`set_host`]: `plugin_toolkit::secrets`
    /// otherwise opens its own connection to run the core secrets SQL, racing the
    /// daemon's on the WAL/shm index (SHMOPEN 5898). Called once, right after
    /// `set_host`. Optional tail field with a no-op default so a plugin built
    /// against an older toolkit keeps its own `open_db` path unchanged.
    #[sabi(missing_field(with = default_set_secret_op))]
    pub set_secret_op: extern "C" fn(secret_op: HostSecretOp),
}

/// Default accessor for [`PluginMod::set_host`] when a plugin predates the
/// field: yields a function that ignores the host services (the old plugin
/// opens its own db, unchanged).
fn default_set_host() -> extern "C" fn(HostDbOp) {
    extern "C" fn noop(_db_op: HostDbOp) {}
    noop
}

/// Default accessor for [`PluginMod::set_secret_op`] when a plugin predates the
/// field: a no-op, so the old plugin keeps its own secrets/db path.
fn default_set_secret_op() -> extern "C" fn(HostSecretOp) {
    extern "C" fn noop(_secret_op: HostSecretOp) {}
    noop
}

/// Default accessor for [`PluginMod::schemas`] when a plugin predates the field:
/// yields a function returning an empty `SchemaDecl` JSON (no namespace, no
/// tables), so "didn't export" is identical to "declared nothing".
fn default_schemas() -> extern "C" fn() -> RString {
    extern "C" fn empty() -> RString {
        RString::from(r#"{"namespace":"","tables":[]}"#)
    }
    empty
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
/// `Default` + per-field `#[serde(default)]` make this struct **forward-
/// compatible**: a plugin constructs it with `..Default::default()` so adding a
/// new domain axis later (the way `runtime` was added for `deploy_target`)
/// never breaks an existing plugin's struct literal at compile time, and an
/// older serialized `BackendDef` missing the new field still deserializes.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct BackendDef {
    /// Domain registry this backend belongs to, e.g. `"storage"` or
    /// `"deploy_target"`. The loader refuses a `BackendDef` whose domain has no
    /// registered constructor.
    #[serde(default)]
    pub domain: String,
    /// Backend name within its domain (storage: `"nfs"`, `"smb"`). For the
    /// `deploy_target` domain this carries the **host** axis (the machine, e.g.
    /// `"host-a"`, `"host-b"`) — one of the three discrete identity axes, never a
    /// flattened `host-runtime` token. Used as (part of) the registry key;
    /// re-registering the same identity replaces in place.
    #[serde(default)]
    pub name: String,
    /// Coarse kind string, domain-interpreted. storage: `network_share` /
    /// `disk_storage` / `object`. deploy_target: the **kind** axis — how orca
    /// manages the workload on its runtime (`cli` / `dockge` / `compose` /
    /// `proxmox` / `quadlet`). Deserialized into the domain's own enum by the
    /// domain constructor.
    #[serde(default)]
    pub kind: String,
    /// The deploy_target **runtime** axis — what actually executes the workload
    /// (`docker` / `podman` / `lxc` / `vm`). Independent of `kind` and the host;
    /// together they form the `(host, runtime, kind)` composite identity. Empty
    /// for domains (storage, notifications, …) that don't use it.
    #[serde(default)]
    pub runtime: String,
    /// Non-secret endpoint string for display, e.g. `nfs://10.0.0.5:/export`.
    #[serde(default)]
    pub endpoint: String,
    /// Capability strings this backend advertises, domain-interpreted (storage:
    /// `list` / `mount` / `unmount` / `usage` / `recover_stale` / …;
    /// deploy_target: `launch` / `stop` / `restart` / `logs` / `shell` /
    /// `metrics` / `snapshot` / `migrate`).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Tool-name prefix the proxy uses when calling back through `invoke`. The
    /// proxy invokes `"{invoke_prefix}.{op}"` (e.g. `"nfs.recover_stale"`) with
    /// the operation's JSON args. Lets one plugin host several backends that
    /// each map to a distinct tool family.
    #[serde(default)]
    pub invoke_prefix: String,
}

// ── Plugin-declared SQL schema (the `schemas()` ABI fn payload) ───────────────
//
// Pure serde types so a THIN plugin (no rusqlite / `db` feature) can declare its
// tables: the descriptor lives in the ABI contract crate, and `db` consumes it
// to materialize real SQL tables. The plugin declares the shape; orca owns the
// connection and performs the migration into the plugin's isolated namespace.

/// One column in a plugin-declared table. Real typed column — NOT JSONB/KV.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    /// SQLite storage class: `TEXT` / `INTEGER` / `REAL` / `BLOB` / `NUMERIC`.
    pub sql_type: String,
    #[serde(default)]
    pub not_null: bool,
    #[serde(default)]
    pub primary_key: bool,
    /// Literal SQL default; required for a `not_null` column added to a table
    /// that may already hold rows.
    #[serde(default)]
    pub default: Option<String>,
}

/// One index over a plugin-declared table.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
}

/// A full declared table: logical name (within the plugin's namespace) + its
/// columns and indexes.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDef {
    pub table: String,
    pub columns: Vec<ColumnDef>,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
}

/// The whole `schemas()` payload: the plugin's namespace plus every table it
/// declares. orca applies each table into `plug__<namespace>__<table>` with a
/// safe additive diff-migration on load.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaDecl {
    /// The plugin's data namespace — the isolation key. A plugin reads/writes
    /// only within this namespace; it can never name another plugin's or a core
    /// table.
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub tables: Vec<TableDef>,
}

// ── Host DB service (the `set_host` channel + `db_op` payload) ─────────────────
//
// "The plugin declares; orca owns the db and performs every operation." Plugins
// NEVER open their own SQLite connection (a second connection to the encrypted
// db races the daemon's on the WAL/shm index → SQLITE_IOERR_SHMOPEN). Instead
// the loader hands each plugin a single `db_op` function pointer bound to core's
// one serialized connection; the toolkit routes every generated CRUD call
// through it as a TYPED op. These are pure serde types — a THIN plugin carries
// them without linking `rusqlite`.

/// The host `db_op` function pointer, wrapped so it can be a `set_host`
/// parameter: the plugin sends a JSON-encoded [`DbOp`] and gets back a
/// JSON-encoded [`DbReply`] on success (or a human-readable error string). Core
/// runs the op on its single pooled connection, so no plugin ever opens a
/// second connection. (`abi_stable` forbids a *nested* bare fn pointer as a
/// parameter, hence the `#[repr(transparent)]` newtype.)
#[repr(transparent)]
#[derive(StableAbi, Copy, Clone)]
pub struct HostDbOp {
    pub func: extern "C" fn(op_json: RStr<'_>) -> RResult<RString, RString>,
}

/// A single SQLite cell value, carried typed across the FFI JSON boundary — no
/// opaque `serde_json::Value`. Maps 1:1 to `rusqlite::types::Value`
/// (`Bool` binds/reads as `INTEGER 0/1`; `Blob` JSON-encodes as a byte array).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum DbValue {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
    Blob(Vec<u8>),
}

/// One row: ordered column name → typed value.
pub type DbRow = std::collections::BTreeMap<String, DbValue>;

/// A typed CRUD operation a plugin asks core to perform on a table within the
/// plugin's own `namespace` (core resolves it to `plug__<namespace>__<table>`
/// and refuses any other table, so a plugin can never touch core or another
/// plugin's tables). This is the whole DB surface a plugin has.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DbOp {
    /// All rows of the table, in insertion order.
    List { namespace: String, table: String },
    /// The single row whose `key_col` equals `key`, if any.
    Get {
        namespace: String,
        table: String,
        key_col: String,
        key: String,
    },
    /// Insert a row; errors on PK/UNIQUE conflict.
    Insert {
        namespace: String,
        table: String,
        row: DbRow,
    },
    /// Update the row identified by `key_col`; `affected` reports whether a row
    /// matched.
    Update {
        namespace: String,
        table: String,
        key_col: String,
        row: DbRow,
    },
    /// Insert or replace on PK/UNIQUE conflict.
    Upsert {
        namespace: String,
        table: String,
        row: DbRow,
    },
    /// Delete the row whose `key_col` equals `key`; `affected` reports whether a
    /// row matched.
    Delete {
        namespace: String,
        table: String,
        key_col: String,
        key: String,
    },
}

/// The reply to a [`DbOp`]. `rows` carries results for `List`/`Get` (0..1 for
/// `Get`); `affected` carries the changed-row count for writes.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct DbReply {
    #[serde(default)]
    pub rows: Vec<DbRow>,
    #[serde(default)]
    pub affected: u64,
}

// ── Host secrets service (the `set_secret_op` channel + `secret_op` payload) ───
//
// Secrets carry crypto (inline values are encrypted with the host key) and their
// tables are core tables, so — unlike per-plugin `DbOp` — the whole operation
// runs in core; the plugin sends a typed [`SecretOp`] and gets a [`SecretReply`].
// This keeps `plugin_toolkit::secrets` from opening its own connection.

/// The host `secret_op` function pointer, wrapped (abi_stable forbids a nested
/// bare fn pointer as a `set_secret_op` parameter).
#[repr(transparent)]
#[derive(StableAbi, Copy, Clone)]
pub struct HostSecretOp {
    pub func: extern "C" fn(op_json: RStr<'_>) -> RResult<RString, RString>,
}

/// A secrets operation a plugin asks core to perform on its behalf, on core's
/// single pooled connection. Mirrors `plugin_toolkit::secrets`' surface.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SecretOp {
    /// Resolve `name` to its value (`None` if unregistered). Core performs the
    /// backend resolution (inline decrypt; external backends error).
    Get { name: String },
    /// Create/replace an inline secret.
    Set {
        name: String,
        value: String,
        description: Option<String>,
    },
    /// Whether a secret with this name is registered.
    Exists { name: String },
    /// Remove a secret (inline value zeroed). `found` reports whether one existed.
    Delete { name: String },
}

/// The reply to a [`SecretOp`]. `value` carries the resolved secret for `Get`
/// (absent → `None`); `found` carries the boolean for `Exists`/`Delete`.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct SecretReply {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub found: bool,
}

/// An HTTP request a plugin asks core to perform on its behalf — the payload of
/// the `http.request` capability. Core owns the single reqwest/rustls stack; a
/// delegating plugin builds this, sends it over the capability channel, and gets
/// back an [`HttpResponse`] without linking any HTTP/TLS code itself. This is the
/// seam that lets a plugin shed reqwest/rustls/hyper (the bulk of its size).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct HttpRequest {
    /// Uppercase HTTP method (`GET`, `POST`, …).
    pub method: String,
    /// Absolute request URL.
    pub url: String,
    /// Request headers as ordered `(name, value)` pairs (repeats preserved).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Raw request body. Empty = no body.
    #[serde(default)]
    pub body: Vec<u8>,
    /// Per-request timeout in milliseconds. `None` = core's default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Skip TLS verification (self-signed upstreams, e.g. a PVE node's cert).
    #[serde(default)]
    pub insecure: bool,
}

/// The reply to an [`HttpRequest`]. Carries the response for **any** status —
/// core does not treat 4xx/5xx as an error, so the plugin's own client applies
/// its status semantics exactly as it would against a direct connection.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Vec<u8>,
}
