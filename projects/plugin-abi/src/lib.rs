//! Plain-serde plugin capability contract.
//!
//! This crate is the *one* canonical set of capability data types shared by
//! orca core and every external, independently-compiled plugin repo. A plugin
//! now runs as an out-of-process subprocess: these types are (de)serialized
//! over the `plugin-proto` wire and the loader's capability channel — there is
//! no FFI, no `abi_stable`, no cdylib boundary.
//!
//! It is deliberately isolated from `plugin-toolkit`: the contract depends only
//! on `serde` + `schemars`, so a consumer that needs only the wire types (the
//! loader, a thin plugin) does not pull orca core. `plugin-toolkit` re-exports
//! this crate as `plugin_toolkit::abi` for source-compatibility.
//!
//! - The plugin's tool manifest is a JSON array of [`ToolDef`] — the same shape
//!   `dispatch`/`openapi` already expect.
//! - A tool invocation is a tool name + a JSON args blob returning a JSON
//!   result (or an error string); the plugin owns whatever async runtime it
//!   needs internally.
//! - [`DbOp`]/[`SecretOp`]/[`HttpRequest`]/… are the typed capability payloads a
//!   plugin sends to core (and core's replies), so a plugin never opens its own
//!   db connection or links its own HTTP/TLS stack.

use schemars::Schema;

/// JSON shape of a single tool definition in a plugin's tool manifest.
///
/// Defined here (not just documented) so plugin authors build their manifest
/// against a typed struct and the loader deserializes against the same one —
/// one canonical contract, no drift. The schema fields are `schemars::Schema`
/// (a typed, serde-(de)serializable JSON-Schema document) — the same type
/// `schemars::schema_for!` produces.
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

/// JSON shape of a single domain backend a plugin contributes, in the plugin's
/// backends array. The loader's domain dispatch table maps
/// [`BackendDef::domain`] to a `register_from_def` constructor that builds a
/// JSON-proxy backend; the proxy routes each operation back to the plugin over
/// the subprocess wire under [`BackendDef::invoke_prefix`].
///
/// One canonical contract, deserialized identically on both sides.
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

// ── Host DB service (the `db.op` capability payload) ──────────────────────────
//
// "The plugin declares; orca owns the db and performs every operation." Plugins
// NEVER open their own SQLite connection (a second connection to the encrypted
// db races the daemon's on the WAL/shm index → SQLITE_IOERR_SHMOPEN). Instead
// the plugin sends a typed [`DbOp`] over the loader's `db.op` capability channel
// and gets back a [`DbReply`]; core runs it on its one serialized connection.
// These are pure serde types — a THIN plugin carries them without linking
// `rusqlite`.

/// A single SQLite cell value, carried typed over the capability JSON channel —
/// no opaque `serde_json::Value`. Maps 1:1 to `rusqlite::types::Value`
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

// ── Host secrets service (the `secret.op` capability payload) ─────────────────
//
// Secrets carry crypto (inline values are encrypted with the host key) and their
// tables are core tables, so — unlike per-plugin `DbOp` — the whole operation
// runs in core; the plugin sends a typed [`SecretOp`] over the loader's
// `secret.op` capability channel and gets a [`SecretReply`]. This keeps
// `plugin_toolkit::secrets` from opening its own connection.

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

/// A STREAMING HTTP request — the payload of the `http.stream` capability. Same
/// request shape as [`HttpRequest`], but core does NOT buffer the response body:
/// it drives reqwest's `bytes_stream()` and relays each byte chunk to the plugin
/// as a `CapStreamChunk` (an [`HttpStreamChunk`]) as it arrives, then a
/// `CapStreamEnd`. This is the seam for large downloads and long-lived event
/// streams (SSE) where buffering the whole body host-side is wrong. A plugin
/// links no reqwest/hyper and still consumes a true stream.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct HttpStreamRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Vec<u8>,
    /// Whole-stream timeout in milliseconds (from send to final chunk). `None` =
    /// core's default. A per-chunk idle timeout is core's concern.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub insecure: bool,
}

/// The FIRST `CapStreamChunk` of an `http.stream` response: the status line and
/// headers, before any body byte. `seq == 0` always carries this variant so the
/// plugin learns the status/headers before the body chunks (`seq >= 1`) arrive.
/// Modeled as an enum so one `data` payload type covers head + body + trailer.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HttpStreamChunk {
    /// Response status + headers (the stream head; always `seq == 0`).
    Head {
        status: u16,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    /// One slice of the response body, in wire order (`seq >= 1`).
    Body {
        #[serde(default)]
        bytes: Vec<u8>,
    },
}

/// The composition a subprocess plugin pushes into core's `agents` domain over
/// the `agents.register` capability. A plugin builds its agents/hooks/skills/
/// commands/prompt-fragments as the `agents` crate's registry defs, serializes
/// each into a JSON array string, and sends this struct; the loader's host
/// handler parses each array back into the registry's `Vec<AgentDef>` etc. and
/// registers a provider under [`AgentRegistration::name`].
///
/// The vecs ride as pre-serialized JSON strings (not typed) on purpose: it keeps
/// `plugin-abi` free of any dependency on the `agents` crate — the same
/// JSON-proxy convention the backend thunks and `DbOp`/`SecretOp` payloads use.
/// `Default` + per-field `#[serde(default)]` keep it forward-compatible.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct AgentRegistration {
    /// Provider name the registration is keyed under; re-registering the same
    /// name replaces the provider in place.
    #[serde(default)]
    pub name: String,
    /// JSON array string of the registry's `AgentDef`s.
    #[serde(default)]
    pub agents_json: String,
    /// JSON array string of the registry's `HookDef`s.
    #[serde(default)]
    pub hooks_json: String,
    /// JSON array string of the registry's `SkillDef`s.
    #[serde(default)]
    pub skills_json: String,
    /// JSON array string of the registry's `CommandDef`s.
    #[serde(default)]
    pub commands_json: String,
    /// JSON array string of the registry's `PromptFragment`s.
    #[serde(default)]
    pub prompt_fragments_json: String,
}
