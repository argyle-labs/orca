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

use schemars::{JsonSchema, Schema};

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
    /// Role required to invoke the tool (`any`/`read`/`admin`); `None` from a
    /// plugin built before the manifest carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Whether the tool executes by default and plans under `dryRun: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execute_gated: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run_aware: bool,
    /// Whether the tool writes to an external managed system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_mutation: Option<bool>,
    /// Whether the tool is refused over `mesh/exec`, so a paired peer cannot
    /// run it on this host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reaches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_callable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_admin: Option<bool>,
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
    /// storage **mount-style** axis: how the backend's mounts are realized on the
    /// host. Empty or `"kernel_mount"` = a kernel mount driven through autofs (nfs,
    /// smb — the default); `"userspace_process"` = realized by a helper process the
    /// backend supervises (a future object-store gateway). Empty for domains that
    /// don't mount. Forward-compatible: an older serialized `BackendDef` missing
    /// this field deserializes as empty (= kernel).
    #[serde(default)]
    pub mount_style: String,
    /// storage **net-fstypes** axis: the kernel filesystem-type strings this
    /// backend's mounts appear as in the mount table (nfs: `["nfs4","nfs"]`, smb:
    /// `["cifs","smbfs"]`). Core unions these across registered backends to learn
    /// which mount-table entries are network shares — so no fstype literal lives in
    /// core. Empty for domains that don't mount and for a disk/object backend.
    /// Forward-compatible: an older serialized `BackendDef` missing it deserializes
    /// as empty.
    #[serde(default)]
    pub net_fstypes: Vec<String>,
    /// storage **default-source-port** axis: the TCP port core probes to test a
    /// source this backend owns (nfs: `2049`, smb: `445`). The port is fstype
    /// grammar the plugin holds, not core — core resolves it from the backend
    /// owning the fstype before probing. `None`/absent for domains that don't
    /// mount and for a backend with no probe transport (disk/object).
    /// Forward-compatible: an older serialized `BackendDef` missing it
    /// deserializes as `None`.
    #[serde(default)]
    pub default_source_port: Option<u16>,
    /// Tool-name prefix the proxy uses when calling back through `invoke`. The
    /// proxy invokes `"{invoke_prefix}.{op}"` (e.g. `"nfs.recover_stale"`) with
    /// the operation's JSON args. Lets one plugin host several backends that
    /// each map to a distinct tool family.
    #[serde(default)]
    pub invoke_prefix: String,
    /// deploy_target **provisioning** axis: the typed, per-runtime placement +
    /// sizing profile a deploy target carries (Proxmox: node / endpoint /
    /// storage / cores / memory). Distinct from a workload's *what* (image,
    /// env, mounts): this is the *where/how* orca provisions a VM/LXC on the
    /// target. Core owns the shape (a variant per runtime) — no plugin-owned
    /// kind, no opaque JSON. `None`/absent for domains that don't provision and
    /// for a container target that needs no sizing. Forward-compatible: an older
    /// serialized `BackendDef` missing it deserializes as `None`.
    #[serde(default)]
    pub provisioning: Option<ProvisioningConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reaches: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<String>,
}

/// A typed, per-runtime provisioning profile a deploy target advertises: the
/// "where/how" a workload is placed and sized on that runtime, orthogonal to a
/// [`WorkloadSpec`]'s "what". Core owns this shape — a variant per runtime, so a
/// plugin never smuggles an opaque JSON blob or a plugin-owned kind through
/// `BackendDef`. The `deploy_target` crate re-exports these types as its domain
/// config (no duplicate shape); the loader threads a plugin's advertised value
/// straight through registration to the registered target.
#[derive(serde::Serialize, serde::Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningConfig {
    /// Provisioning parameters for a Proxmox VM/LXC target (`pct`/`qm`).
    Proxmox(ProxmoxProvisioning),
}

/// Placement + sizing for a Proxmox VM/LXC deploy target. Carries exactly what a
/// Proxmox provision needs beyond the generic [`WorkloadSpec`]: which node and
/// endpoint to provision on, which storage to allocate the rootfs/disk from, and
/// the CPU/memory sizing profile.
#[derive(
    serde::Serialize, serde::Deserialize, JsonSchema, Debug, Clone, Default, PartialEq, Eq,
)]
pub struct ProxmoxProvisioning {
    /// Proxmox node the guest is provisioned on (e.g. `pve`).
    pub node: String,
    /// Cluster/API endpoint the node is reached through (non-secret).
    pub endpoint: String,
    /// Storage pool the rootfs / disk is allocated from (e.g. `local-lvm`).
    pub storage: String,
    /// CPU cores to allocate.
    pub cores: u32,
    /// Memory to allocate, in megabytes.
    pub memory_mb: u64,
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
    #[serde(default)]
    pub secret: bool,
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

impl DbOp {
    /// The `namespace` this op targets. Every variant carries one; the loader
    /// checks it against the calling plugin's principal so a plugin can only
    /// reach its own `plug__<namespace>__*` tables.
    pub fn namespace(&self) -> &str {
        match self {
            DbOp::List { namespace, .. }
            | DbOp::Get { namespace, .. }
            | DbOp::Insert { namespace, .. }
            | DbOp::Update { namespace, .. }
            | DbOp::Upsert { namespace, .. }
            | DbOp::Delete { namespace, .. } => namespace,
        }
    }

    /// Static op-kind label for diagnostics (e.g. cross-namespace warnings).
    pub fn kind(&self) -> &'static str {
        match self {
            DbOp::List { .. } => "list",
            DbOp::Get { .. } => "get",
            DbOp::Insert { .. } => "insert",
            DbOp::Update { .. } => "update",
            DbOp::Upsert { .. } => "upsert",
            DbOp::Delete { .. } => "delete",
        }
    }
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

impl SecretOp {
    /// The secret `name` this op targets. The loader checks it is scoped to the
    /// calling plugin's principal (prefixed `<principal>.`) so a plugin can only
    /// reach its own secrets, never another plugin's or a core secret.
    pub fn name(&self) -> &str {
        match self {
            SecretOp::Get { name }
            | SecretOp::Set { name, .. }
            | SecretOp::Exists { name }
            | SecretOp::Delete { name } => name,
        }
    }

    /// Static op-kind label for diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            SecretOp::Get { .. } => "get",
            SecretOp::Set { .. } => "set",
            SecretOp::Exists { .. } => "exists",
            SecretOp::Delete { .. } => "delete",
        }
    }
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
    #[serde(default, with = "b64_or_bytes")]
    pub body: Vec<u8>,
    /// Per-request timeout in milliseconds. `None` = core's default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Skip TLS verification (self-signed upstreams, e.g. a PVE node's cert).
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub max_body_bytes: Option<u64>,
    /// The plugin accepts a base64 `HttpResponse.body`; core replies with a
    /// number array otherwise.
    #[serde(default)]
    pub body_b64: bool,
    /// `Some(false)` returns the first response (e.g. a 302 + `Set-Cookie`)
    /// unfollowed; `None` follows redirects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
}

/// Capability name core advertises in `Welcome` when it accepts a base64
/// `HttpRequest.body`.
pub const HTTP_BODY_B64: &str = "http.body_b64";

pub const HTTP_NO_FOLLOW: &str = "http.no_follow";

pub const LXC_EXEC: &str = "lxc.exec";

pub const LXC_PUSH: &str = "lxc.push";

pub const PRIVILEGED_APPLY: &str = "privileged.apply";

pub const TOOL_CALL: &str = "tool.call";

/// Raw outcome of a root helper the daemon ran for a privileged capability.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct HelperOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Borrowed wire form of an [`HttpRequest`] whose body is a number array, for
/// a core that does not advertise [`HTTP_BODY_B64`].
#[derive(serde::Serialize)]
pub struct LegacyHttpRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(String, String)],
    #[serde(serialize_with = "bytes_as_seq")]
    pub body: &'a [u8],
    pub timeout_ms: Option<u64>,
    pub insecure: bool,
    pub max_body_bytes: Option<u64>,
    pub body_b64: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_redirects: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
}

impl HttpRequest {
    pub fn legacy(&self) -> LegacyHttpRequest<'_> {
        LegacyHttpRequest {
            method: &self.method,
            url: &self.url,
            headers: &self.headers,
            body: &self.body,
            timeout_ms: self.timeout_ms,
            insecure: self.insecure,
            max_body_bytes: self.max_body_bytes,
            body_b64: self.body_b64,
            follow_redirects: self.follow_redirects,
            connect_timeout_ms: self.connect_timeout_ms,
            retry: self.retry,
        }
    }
}

/// Borrowed wire form of an [`HttpResponse`] whose body is a number array, for
/// a plugin that did not set [`HttpRequest::body_b64`].
#[derive(serde::Serialize)]
pub struct LegacyHttpResponse<'a> {
    pub status: u16,
    pub headers: &'a [(String, String)],
    #[serde(serialize_with = "bytes_as_seq")]
    pub body: &'a [u8],
}

impl HttpResponse {
    pub fn legacy(&self) -> LegacyHttpResponse<'_> {
        LegacyHttpResponse {
            status: self.status,
            headers: &self.headers,
            body: &self.body,
        }
    }
}

fn bytes_as_seq<S: serde::Serializer>(bytes: &&[u8], s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(bytes.iter())
}

/// The reply to an [`HttpRequest`]. Carries the response for **any** status —
/// core does not treat 4xx/5xx as an error, so the plugin's own client applies
/// its status semantics exactly as it would against a direct connection.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, with = "b64_or_bytes")]
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

// ── Host-brokered WebSocket (the `ws.*` capability payloads) ─────────────────
//
// Core owns the WebSocket/TLS stack; a plugin opens a session, then pulls
// inbound messages and pushes outbound ones by session id. Inbound delivery is
// a pull (`ws.recv`) because the capability wire is serial: a plugin cannot
// issue a `ws.send` while it is inside a host-driven stream.

/// Open a WebSocket — the payload of the `ws.open` capability. The reply is a
/// [`WsChunk::Open`] carrying the session id every other `ws.*` op names.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WsOpen {
    /// Absolute `ws://` or `wss://` URL.
    pub url: String,
    /// Handshake request headers as ordered `(name, value)` pairs.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Skip TLS verification (self-signed upstreams).
    #[serde(default)]
    pub insecure: bool,
    /// Connect + handshake timeout in milliseconds. `None` = core's default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Largest inbound message (and single frame) to accept, in bytes. `None` =
    /// core's default (1 MiB). Core clamps it to 16 MiB. A larger message
    /// fails the session.
    #[serde(default)]
    pub max_message_bytes: Option<u64>,
}

/// One event on a WebSocket session: the `ws.open` reply ([`WsChunk::Open`]) or
/// one inbound message (the `ws.recv` reply; `null` there means the peer closed).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WsChunk {
    Open {
        session: u64,
    },
    Text {
        text: String,
    },
    Binary {
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
}

/// Wait for the next inbound message — the payload of the `ws.recv` capability.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WsRecv {
    pub session: u64,
    /// Give up (error, session kept) after this many milliseconds. `None` =
    /// core's default (30s). Core clamps it to 30s and to the time left before
    /// the enclosing tool invocation's ceiling.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// An outbound message body for [`WsSend`].
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WsPayload {
    Text(String),
    Binary(#[serde(with = "b64")] Vec<u8>),
}

/// Send one message — the payload of the `ws.send` capability.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WsSend {
    pub session: u64,
    #[serde(flatten)]
    pub payload: WsPayload,
}

/// Close a session — the payload of the `ws.close` capability. `code` defaults
/// to 1000 (normal closure).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WsClose {
    pub session: u64,
    #[serde(default)]
    pub code: Option<u16>,
}

/// Standard-alphabet padded base64 for binary WebSocket payloads, hand-rolled so
/// this crate keeps only its serde + schemars deps.
mod b64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    const DECODE: [u8; 256] = {
        let mut t = [0xFF; 256];
        let mut i = 0;
        while i < 64 {
            t[ALPHABET[i] as usize] = i as u8;
            i += 1;
        }
        t
    };

    pub fn serialize<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text: String = serde::Deserialize::deserialize(d)?;
        decode(&text).ok_or_else(|| serde::de::Error::custom("invalid base64"))
    }

    pub(super) fn decode(text: &str) -> Option<Vec<u8>> {
        let raw = text.as_bytes();
        if !raw.len().is_multiple_of(4) {
            return None;
        }
        let mut out = Vec::with_capacity(raw.len() / 4 * 3);
        for quad in raw.chunks(4) {
            let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
            if pad > 2 {
                return None;
            }
            let mut n = 0u32;
            for (i, &c) in quad.iter().enumerate() {
                let v = if i >= 4 - pad {
                    0
                } else {
                    match DECODE[c as usize] {
                        0xFF => return None,
                        v => u32::from(v),
                    }
                };
                n |= v << (18 - 6 * i);
            }
            out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
        }
        Some(out)
    }
}

/// Serializes as [`b64`]; deserializes either base64 or a legacy number array.
mod b64_or_bytes {
    pub use super::b64::serialize;

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        d.deserialize_any(Visitor)
    }

    struct Visitor;

    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<u8>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a base64 string or an array of bytes")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
            super::b64::decode(v).ok_or_else(|| E::custom("invalid base64"))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(b) = seq.next_element::<u8>()? {
                out.push(b);
            }
            Ok(out)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Vec<u8>, E> {
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_def_dry_run_aware_round_trips_and_defaults_off() {
        let base =
            json!({"name": "p.x", "description": "d", "input_schema": {}, "output_schema": {}});
        let old: ToolDef = serde_json::from_value(base.clone()).unwrap();
        assert!(!old.dry_run_aware);
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("dry_run_aware")
                .is_none()
        );
        let mut aware = base;
        aware["dry_run_aware"] = json!(true);
        let def: ToolDef = serde_json::from_value(aware).unwrap();
        assert!(def.dry_run_aware);
        let back: ToolDef = serde_json::from_value(serde_json::to_value(&def).unwrap()).unwrap();
        assert!(back.dry_run_aware);
    }

    #[test]
    fn http_response_body_serializes_base64() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![],
            body: b"hello".to_vec(),
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap()["body"],
            json!("aGVsbG8=")
        );
    }

    #[test]
    fn http_response_body_accepts_base64_and_legacy_array() {
        let b64: HttpResponse =
            serde_json::from_value(json!({"status": 200, "body": "aGVsbG8="})).unwrap();
        let legacy: HttpResponse =
            serde_json::from_value(json!({"status": 200, "body": [104, 101, 108, 108, 111]}))
                .unwrap();
        let absent: HttpResponse = serde_json::from_value(json!({"status": 204})).unwrap();
        assert_eq!(b64.body, b"hello");
        assert_eq!(legacy.body, b"hello");
        assert!(absent.body.is_empty());
    }

    #[test]
    fn http_response_body_rejects_bad_input() {
        assert!(
            serde_json::from_value::<HttpResponse>(json!({"status": 200, "body": "a"})).is_err()
        );
        assert!(
            serde_json::from_value::<HttpResponse>(json!({"status": 200, "body": [256]})).is_err()
        );
    }

    #[test]
    fn legacy_forms_match_typed_shape_with_array_body() {
        let req = HttpRequest {
            method: "POST".into(),
            url: "http://x/".into(),
            headers: vec![("a".into(), "b".into())],
            body: vec![1, 2, 255],
            timeout_ms: Some(5),
            insecure: true,
            max_body_bytes: Some(9),
            body_b64: true,
            follow_redirects: Some(false),
            connect_timeout_ms: None,
            retry: None,
        };
        let mut typed = serde_json::to_value(&req).unwrap();
        typed["body"] = json!([1, 2, 255]);
        assert_eq!(serde_json::to_value(req.legacy()).unwrap(), typed);
        let resp = HttpResponse {
            status: 201,
            headers: vec![("c".into(), "d".into())],
            body: vec![0, 7],
        };
        let mut typed = serde_json::to_value(&resp).unwrap();
        typed["body"] = json!([0, 7]);
        assert_eq!(serde_json::to_value(resp.legacy()).unwrap(), typed);
    }

    #[test]
    fn base64_decoder_rejects_malformed_and_round_trips_all_bytes() {
        for bad in ["a", "ab=c", "a===", "ab!d", "=abc", "\u{e9}ab"] {
            assert!(
                serde_json::from_value::<HttpResponse>(json!({"status": 200, "body": bad}))
                    .is_err(),
                "{bad}"
            );
        }
        let all: Vec<u8> = (0..=255).collect();
        for n in 0..4 {
            let resp = HttpResponse {
                status: 200,
                headers: vec![],
                body: all[n..].to_vec(),
            };
            let back: HttpResponse =
                serde_json::from_value(serde_json::to_value(&resp).unwrap()).unwrap();
            assert_eq!(back.body, resp.body);
        }
    }

    #[test]
    fn http_request_body_and_flag_round_trip() {
        let legacy: HttpRequest = serde_json::from_value(
            json!({"method": "POST", "url": "http://x/", "body": [1, 2, 255]}),
        )
        .unwrap();
        assert_eq!(legacy.body, vec![1, 2, 255]);
        assert!(!legacy.body_b64);
        assert_eq!(legacy.follow_redirects, None);
        let wire = serde_json::to_value(HttpRequest {
            body_b64: true,
            ..legacy
        })
        .unwrap();
        assert_eq!(wire["body"], json!("AQL/"));
        let back: HttpRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(back.body, vec![1, 2, 255]);
        assert!(back.body_b64);
    }

    #[test]
    fn follow_redirects_is_omitted_by_default_and_round_trips() {
        let req: HttpRequest =
            serde_json::from_value(json!({"method": "GET", "url": "http://x/"})).unwrap();
        assert!(
            serde_json::to_value(&req)
                .unwrap()
                .get("follow_redirects")
                .is_none()
        );
        assert!(
            serde_json::to_value(req.legacy())
                .unwrap()
                .get("follow_redirects")
                .is_none()
        );
        let wire = serde_json::to_value(HttpRequest {
            follow_redirects: Some(false),
            ..req
        })
        .unwrap();
        assert_eq!(wire["follow_redirects"], json!(false));
        let back: HttpRequest = serde_json::from_value(wire).unwrap();
        assert_eq!(back.follow_redirects, Some(false));
    }

    #[test]
    fn response_with_repeated_set_cookie_keeps_old_shape() {
        let old: HttpResponse = serde_json::from_value(json!({
            "status": 302,
            "headers": [["location", "/"], ["set-cookie", "a=1"], ["set-cookie", "b=2"]],
            "body": []
        }))
        .unwrap();
        assert_eq!(old.headers.len(), 3);
        let wire = serde_json::to_value(old.legacy()).unwrap();
        assert_eq!(wire["headers"][2], json!(["set-cookie", "b=2"]));
        let back: HttpResponse = serde_json::from_value(wire).unwrap();
        assert_eq!(back.headers, old.headers);
        let bare: HttpResponse = serde_json::from_value(json!({"status": 204})).unwrap();
        assert!(bare.headers.is_empty());
    }

    #[test]
    fn ws_open_defaults_optional_fields() {
        let open: WsOpen = serde_json::from_value(json!({"url": "wss://h/ws"})).unwrap();
        assert_eq!(
            open,
            WsOpen {
                url: "wss://h/ws".into(),
                headers: vec![],
                insecure: false,
                timeout_ms: None,
                max_message_bytes: None,
            }
        );
    }

    #[test]
    fn ws_chunk_wire_shape() {
        assert_eq!(
            serde_json::to_value(WsChunk::Open { session: 7 }).unwrap(),
            json!({"t": "open", "session": 7})
        );
        assert_eq!(
            serde_json::to_value(WsChunk::Text { text: "hi".into() }).unwrap(),
            json!({"t": "text", "text": "hi"})
        );
        assert_eq!(
            serde_json::to_value(WsChunk::Binary {
                bytes: b"hello".to_vec()
            })
            .unwrap(),
            json!({"t": "binary", "bytes": "aGVsbG8="})
        );
    }

    #[test]
    fn ws_send_wire_shape() {
        let text = WsSend {
            session: 1,
            payload: WsPayload::Text("ping".into()),
        };
        assert_eq!(
            serde_json::to_value(&text).unwrap(),
            json!({"session": 1, "text": "ping"})
        );
        let bin = WsSend {
            session: 2,
            payload: WsPayload::Binary(vec![0, 255]),
        };
        let v = serde_json::to_value(&bin).unwrap();
        assert_eq!(v, json!({"session": 2, "binary": "AP8="}));
        assert_eq!(serde_json::from_value::<WsSend>(v).unwrap(), bin);
    }

    #[test]
    fn ws_close_and_recv_default_optional_fields() {
        let close: WsClose = serde_json::from_value(json!({"session": 3})).unwrap();
        assert_eq!(
            close,
            WsClose {
                session: 3,
                code: None
            }
        );
        let recv: WsRecv = serde_json::from_value(json!({"session": 3})).unwrap();
        assert_eq!(
            recv,
            WsRecv {
                session: 3,
                timeout_ms: None
            }
        );
    }

    #[test]
    fn base64_round_trips_every_padding_length() {
        for len in 0..=8u8 {
            let bytes: Vec<u8> = (0..len).map(|i| i.wrapping_mul(97)).collect();
            let chunk = WsChunk::Binary { bytes };
            let back: WsChunk =
                serde_json::from_value(serde_json::to_value(&chunk).unwrap()).unwrap();
            assert_eq!(back, chunk);
        }
    }

    #[test]
    fn base64_rejects_garbage() {
        for bad in ["abc", "a!==", "a==="] {
            assert!(
                serde_json::from_value::<WsChunk>(json!({"t": "binary", "bytes": bad})).is_err(),
                "{bad}"
            );
        }
    }
}
