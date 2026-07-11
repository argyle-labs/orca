//! `orca-plugin-toolkit` — higher-level primitives for plugin authors.
//!
//! Not a re-export shim. The toolkit COMBINES the underlying macros so a
//! plugin expresses maximum functionality with minimum boilerplate (see
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]]). One toolkit
//! macro emits db table + REST verb tools + endpoint resolution + serde +
//! clap + MCP + REST + schemars in one shot.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use plugin_toolkit::prelude::*;
//!
//! endpoint_resource! {
//!     plugin: "dockge",
//!     fields: {
//!         base_url: String,
//!         token:    String,
//!     }
//! }
//! ```
//!
//! Generates `dockge.{list,detail,create,update,delete}` over a
//! `dockge_endpoints` SQLite table — every surface (CLI / MCP / REST) is
//! automatic. No further code is needed for the registry layer; plugin
//! authors only hand-write upstream API logic and surface-extension tools
//! (e.g. stack lifecycle).

pub mod abi;
pub mod address;
#[cfg(all(feature = "http", not(feature = "delegated-http")))]
pub mod api_client;
/// Out-of-process capability sink (dependency-light: no rusqlite). Home of
/// `with_cap_sink` + `http_request`, so the delegated-HTTP shim reaches the
/// capability channel without the `db` feature.
pub mod capsink;
/// Data-driven endpoint executor: run a whole REST/OpenAPI surface from an
/// embedded [`descriptor::EndpointDescriptor`] table + one shared validating
/// executor, instead of a compiled tool fn + type per operation. The thinnest
/// form an API-client plugin can take. Dependency-free (validator hand-rolled,
/// transport via [`capsink`]).
#[cfg(feature = "descriptor")]
pub mod descriptor;
/// cdylib export glue. Its `runtime()` drives a plugin's async backend behind
/// the synchronous FFI `invoke`, so it carries the tokio reactor and is gated on
/// `in-process`. A thin subprocess plugin serves through [`serve`] instead.
#[cfg(feature = "in-process")]
pub mod export;
/// Async byte-sink helpers (write-to + shutdown an executor-produced writer,
/// e.g. a bollard exec stdin) so a plugin never names the executor's
/// `AsyncWriteExt`. Reactor-bound but always available — the reactor is the
/// shared orca-owned surface (see [`reactor`]).
pub mod io;
/// Async subprocess exec helpers for deploy-lifecycle tool surfaces
/// (`tokio::process::Command`, internal). Runs on the shared reactor, so it is
/// always available — a plugin reaches exec here without naming the runtime.
pub mod lifecycle;
pub mod logging;
pub mod prelude;
/// Async subprocess utility (orca-owned surface; the runtime is internal). A
/// plugin spawns processes here instead of naming the executor's process API.
/// Runs on the shared reactor, so it is always available — the generic surface a
/// dynamic plugin registers its subprocess work against.
pub mod process;
/// The shared, orca-owned async reactor (`block_on` / `spawn_detached`) — the
/// generic surface a dynamic plugin registers its async work against. Always
/// available; the runtime is an internal detail plugins never name.
pub mod reactor;
#[cfg(feature = "db")]
pub mod runtime;
/// Abstract, backend-agnostic secrets domain (see [`secrets`]). Gated on `db`:
/// the inline backend and the registry live in the orca db.
#[cfg(feature = "db")]
pub mod secrets;
pub mod serde_ext;
#[cfg(all(feature = "tools", feature = "db"))]
pub mod serve;
/// Generic async Socket.IO client transport (socket-only services like dockge).
#[cfg(feature = "socketio")]
pub mod socketio;
/// Async stream consumption (`next`) — drain a domain client's async stream
/// (bollard logs/exec) without naming the executor's `StreamExt`. Always
/// available; runs on the shared reactor.
pub mod stream;
/// Async time utility (`sleep` / `timeout` / `Deadline`) — orca-owned surface so
/// a plugin awaits without ever naming the runtime. See [`process`]. Always
/// available (runs on the shared reactor); the wall-clock `Timestamp`/`now`
/// re-exports are gated on `tools`.
pub mod time;
/// Thin-profile tool-surface helpers (manifest filtering + `minimal_ctx`) that
/// both the in-process `export` glue and the out-of-process [`serve`] loop need.
/// Reactor-free, so gated on `tools` alone rather than `in-process`.
#[cfg(feature = "tools")]
pub mod tool_manifest;

/// Thin-profile backend-descriptor builders (`unit_backend_def`,
/// `topology_backend_def`, `host_facts_backend_def`, `service_identity_backend_def`
/// and their `*_backends_json` wrappers) that build a [`abi::BackendDef`] from a
/// plugin's contract declarations. Pure — no reactor, no FFI — so gated on
/// `tools` alone, shared by the in-process `export` glue and subprocess plugins.
#[cfg(feature = "tools")]
pub mod backend_def;

/// Filesystem path helpers (`which`, `expand_tilde`). Native to the toolkit —
/// pure `std` with no transitive deps — so the always-on light core provides
/// binary resolution to storage adapters (smb/nfs `which mount.cifs`) without
/// dragging in the `utils` http/git stack. The full profile's other utils
/// re-exports (`http`, `json_schema`) remain feature-gated below.
pub mod path {
    /// Expand a leading `~/` to the user's `$HOME` directory. If `$HOME` is
    /// unset, the tilde is replaced with an empty string.
    pub fn expand_tilde(path: &str) -> String {
        if let Some(rest) = path.strip_prefix("~/") {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/{rest}")
        } else {
            path.to_string()
        }
    }

    /// Locate an executable on `$PATH` via the system `which` command. Returns
    /// the resolved absolute path, or `None` if not found.
    pub fn which(name: &str) -> Option<String> {
        let out = std::process::Command::new("which")
            .arg(name)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() { None } else { Some(path) }
    }
}

// `endpoint_resource!` is a function-like proc-macro defined in the
// `derive` crate (alongside `#[orca_tool]` and `#[derive(Replicated)]`).
// Re-exported here so plugin authors only depend on the toolkit crate.
pub use derive::endpoint_resource;

// ── Macro-emission landing pads ────────────────────────────────────────
//
// `#[orca_tool]` / `endpoint_resource!` / dispatch's `register_op!` emit
// absolute paths like `::plugin_toolkit::contract::OrcaTool`,
// `::plugin_toolkit::inventory::submit!`, `::plugin_toolkit::serde_json::*`.
// Those resolve through the re-exports below, so a consumer crate only
// needs `plugin-toolkit` as a direct dep — never `contract`, `inventory`,
// `serde_json`, `anyhow`, `clap`, `schemars`, `async_trait`,
// `dispatch`, `derive`, `db`, or `rusqlite`. `tokio` is intentionally absent —
// async is reached via `plugin_toolkit::{time, process}`, not the raw runtime.
// See [[feedback-plugin-toolkit-is-the-gateway]]
// and task #29.

pub use ::abi_stable;
pub use ::anyhow;
pub use ::async_trait;
pub use ::clap;
pub use ::inventory;
pub use ::schemars;
pub use ::serde;
pub use ::serde_json;
pub use ::thiserror;
// NB: `tokio` is deliberately NOT re-exported. The runtime is an orca
// implementation detail — plugins reach async through the orca-owned
// `plugin_toolkit::{time, process}` surface and never name the executor. See
// [[plugins-stay-thin]] and [[orca-north-star-abstract-system-differences]].

// ── Gated macro/transport anchors ───────────────────────────────────────
// `#[orca_tool]` / dispatch's `register_op!` emit `::plugin_toolkit::contract`
// + `::plugin_toolkit::dispatch` paths; `endpoint_resource!` adds
// `::plugin_toolkit::{db, rusqlite}`. A plugin that invokes those macros pulls
// the `tools` / `db` features (both in `full`, the default). A storage-only
// adapter that uses only `#[plugin_struct]` never references them, so under
// `default-features = false` they vanish.
#[cfg(feature = "tools")]
pub use ::contract;
// The `db` crate + `rusqlite` are the heavy in-core storage layer
// (reqwest/mysql/postgres/moka + the SQLite driver). Only the in-daemon
// `endpoint_resource!` path references them, so they ride the `db-incore`
// feature — a plugin on the light `db` feature links neither. See
// [[plugins-stay-thin]].
#[cfg(feature = "db-incore")]
pub use ::db;
pub use ::derive;
#[cfg(feature = "tools")]
pub use ::dispatch;
#[cfg(feature = "db-incore")]
pub use ::rusqlite;

// GraphQL query trait + derive. The build-time codegen
// (`plugin_toolkit_build::graphql`) rewrites its emitted `graphql_client::`
// paths to `::plugin_toolkit::graphql_client::*`, so plugins never dep the
// crate directly.
#[cfg(feature = "graphql")]
pub use ::graphql_client;

// OpenAPI / progenitor codegen runtime. The build-time codegen
// (`plugin_toolkit_build::openapi`) rewrites the progenitor-emitted crate
// paths to `::plugin_toolkit::*`, so an OpenAPI plugin needs none of these as
// direct deps.
#[cfg(all(feature = "openapi", not(feature = "delegated-http")))]
pub use ::{bytes, futures_core, progenitor_client, regress};
#[cfg(all(feature = "http", not(feature = "delegated-http")))]
pub use ::{futures_util, reqwest};

// Delegated HTTP: the cap-backed shims stand in for the real crates under the
// SAME names the codegen references (`plugin_toolkit::{reqwest,
// progenitor_client, api_client}`), so a progenitor OpenAPI client executes over
// the `http.request` capability with no codegen change and links no reqwest.
#[cfg(feature = "delegated-http")]
pub mod delegated_http;
#[cfg(feature = "delegated-http")]
pub use delegated_http::{api_client, progenitor_client, reqwest};
// `futures_util` is a light utility (no TLS/http) some plugins use directly
// (e.g. `future::join_all`); re-export it under delegated HTTP too so a thin
// plugin reaches `plugin_toolkit::futures_util` without a real HTTP stack.
#[cfg(feature = "delegated-http")]
pub use ::futures_util;
// Generated string-pattern validation needs `regress` even under delegated HTTP
// (it's small; only reqwest/progenitor-client are shed).
#[cfg(feature = "delegated-http")]
pub use ::regress;

// Macro-runtime registration target types (re-exported so endpoint_resource!
// emissions resolve through plugin_toolkit, not macro_runtime directly). Gated
// with `db`: only `endpoint_resource!` plugins reference these, and the crate
// pulls dispatch (→axum/reqwest) + rusqlite.
// `SchemaFragment` (name + SQL) is light — every `endpoint_resource!` plugin
// emits one and the daemon applies it, so it rides the light `db` feature.
#[cfg(feature = "db")]
pub use ::macro_runtime::SchemaFragment;
// `ReplicatedRegistration` carries `fn(&rusqlite::Connection)` and is emitted
// only by `#[derive(Replicated)]` (a core mesh-sync primitive), so it rides
// `db-incore`. A plugin's `endpoint_resource!` never references it.
#[cfg(feature = "db-incore")]
pub use ::macro_runtime::ReplicatedRegistration;
pub use ::tracing;

// ── Runtime primitives ──────────────────────────────────────────────────
//
// Per [[feedback-plugin-toolkit-is-the-gateway]], plugins reach every
// orca-side capability through the toolkit. These submodules re-export the
// underlying crates so a plugin's only orca-side import is
// `use plugin_toolkit::prelude::*;` — `http`, `graphql`, `openapi`
// are then in scope as namespaced modules. All are feature-gated; the default
// `full` profile provides every one (existing plugins unchanged).

/// HTTP transport. Re-export of `utils::http` so HTTP bug fixes propagate
/// to every plugin from one place.
#[cfg(feature = "http")]
pub mod http {
    pub use utils::http::*;
}

/// JSON Schema node model. Re-export of `utils::json_schema` so plugins that
/// federate or proxy externally-defined tool schemas (e.g. the MCP client)
/// model them through the toolkit rather than direct-dep on `utils`.
#[cfg(feature = "http")]
pub mod json_schema {
    pub use utils::json_schema::*;
}

/// GraphQL client + envelope types. Re-export of the `graphql` crate so
/// plugins talk GraphQL transport without importing the crate directly.
#[cfg(feature = "graphql")]
pub mod graphql {
    pub use ::graphql::*;
    // The query trait the build-time codegen implements for each operation.
    // Re-exported here so a plugin's generic bounds read `graphql::GraphQLQuery`
    // — the plugin never names the backing `graphql_client` crate.
    pub use ::graphql_client::GraphQLQuery;
}

/// OpenAPI spec parsing + normalization helpers. Re-export of the
/// `openapi` crate. Typed-client codegen (progenitor) runs in plugin
/// build scripts — a build-time helper for the codegen pipeline is the
/// next slice on top of this primitive.
#[cfg(feature = "openapi")]
pub mod openapi {
    pub use ::openapi::*;
}

// ── Domain registration crates ──────────────────────────────────────────
//
// Per [[feedback-plugin-toolkit-only-no-exceptions]]: every orca capability,
// including the domain contracts plugins register with, reaches plugins ONLY
// through this gateway. Third-party plugin authors write
// `use plugin_toolkit::prelude::*;` + `use plugin_toolkit::<domain>::*;`
// and never direct-dep on a domain crate. Cycles are broken by relocating
// `#[orca_tool]` sites OUT of the domain crate into a sibling/system crate
// (see `system::notify_send` for the pattern). Domain crates here are
// pure plumbing: model + trait + dispatcher.
/// Notification domain. Exposed to plugins as `notify` (matching the
/// `notify.*` tool namespace); the underlying crate is named `notifications`
/// internally to avoid colliding with the crates.io `notify` fs-watcher crate.
#[cfg(feature = "notify")]
pub mod notify {
    pub use ::notifications::*;
}
#[cfg(feature = "containers")]
pub mod containers {
    pub use ::containers::*;
}
/// Generic storage domain. orca treats every storage provider — NFS/SMB
/// network shares, Proxmox-managed disk storage, … — through one trait + one
/// registry. A plugin contributes facts ("this share is mountable here") and
/// capabilities (mount/unmount/list); orca doesn't care what kind of storage,
/// only that it has access to storage. nfs/smb register network-share backends;
/// proxmox registers an API-managed disk-storage backend.
pub mod storage {
    pub use ::storage::*;
}

/// Generic deploy-target domain. orca treats every place it can run a workload
/// (a Proxmox VM/LXC, a Docker engine, a Dockge host, Podman) through one trait
/// plus one registry. A plugin advertises a target with its kind and
/// capabilities, and orca iterates the registered targets rather than naming
/// runtimes. This is the seam the cross-runtime migration engine builds on: a
/// workload is bound to a target, not pinned to a runtime.
pub mod deploy_target {
    pub use ::deploy_target::*;
}

/// Generic service domain. orca treats every deployable service — media
/// servers, IoT bridges, DNS, reverse proxies, local LLM runners — through one
/// `ServiceBackend` trait + one registry. A plugin contributes a backend; the
/// generic `service.*` tools take the service name as a parameter and drive
/// deploy/backup/restore/configure/status. This keeps the fleet's API surface
/// tiny: N service plugins add 0 tools.
pub mod service {
    pub use ::service::*;
}

// ── Light utility seams, re-exported from `utils` ─────────────────────────
//
// `plugin_toolkit` is the single gateway, and the ONLY crate that re-exports.
// These are orca's OWN abstractions (`utils::{hash,id,url}`) — NOT third-party
// crates — so re-exporting is correct: it single-sources each seam instead of
// duplicating its body here (which would let `plugin_toolkit::url::join` and
// `utils::url::join` drift apart). The `utils` dep is the tree-shaken light
// core (default-features off → no contract/dispatch/tokio/glob), so even the
// thinnest plugin stays thin. Plugins reach `plugin_toolkit::{hash,id,url}`;
// the backing libs (sha2/uuid/urlencoding) never surface.

/// SHA-256 / hex helpers — see [`utils::hash`].
pub use ::utils::hash;
/// Time-ordered ID generation (`new` / `new_short` / `is_valid`) — see [`utils::id`].
pub use ::utils::id;
/// URL percent-encoding + base/path `join` — see [`utils::url`].
pub use ::utils::url;
