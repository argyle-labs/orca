//! The plugin gateway — the *only* import line a plugin source file
//! ever writes:
//!
//! ```rust,ignore
//! use plugin_toolkit::prelude::*;
//! ```
//!
//! Per the user's directive (2026-06-13): "if it isn't in the plugin
//! toolkit, treat it as if it doesn't exist from the plugin perspective."
//! This prelude re-exports every type, trait, derive, and macro a plugin
//! author needs to build a tool surface — `ToolCtx`, `JsonAny`, the
//! serde / clap / schemars derives, `anyhow::Result` + macros, the
//! `#[orca_tool]` attribute, the `endpoint_resource!` declarative macro,
//! and the runtime helpers used by macro-emitted code.
//!
//! Plugin source files MUST NOT import directly from `contract`,
//! `dispatch`, `derive`, `schemars`, `serde`, `inventory`, `clap`, `db`,
//! `rusqlite`, `anyhow`, or `async_trait`. If you find yourself reaching
//! past the prelude, the toolkit is missing a primitive — file a fix.
//!
//! (Cargo.toml deps on those crates remain transitionally because
//! `#[orca_tool]`-emitted code references them by absolute path. A future
//! refactor will route those paths through the toolkit too.)

// ── Trait + type anchors plugin tools build against ─────────────────────
// Gated with the `tools` feature (in the default `full` profile): a tool-
// authoring plugin needs `ToolCtx`/`JsonAny`; a storage-only adapter under
// `default-features = false` never references them and so drops `contract`.
#[cfg(feature = "tools")]
pub use contract::{JsonAny, ToolCtx};

// ── Macros emitted into plugin scope ────────────────────────────────────
// `#[plugin_struct]` / `#[plugin_struct(args)]` for data types (with
// `#[plugin(rename_all = ..., skip_if_none, ...)]` field attributes), and
// `#[plugin_error]` for error enums (`#[plugin(display = "...", from)]`). A
// plugin expresses serialization, schema, CLI, and error behavior entirely
// through these — it never names serde / schemars / clap / thiserror.
pub use derive::{endpoint_resource, orca_tool, plugin_error, plugin_struct};

// ── cdylib export macros ────────────────────────────────────────────────
// One-line cdylib root export: `export_tool_plugin!` (tool surface) /
// `export_storage_plugin!` (storage backend) collapse the whole hand-written
// `abi_export.rs` boilerplate. The shared logic lives in `crate::export`, which
// is `in-process`-only (a cdylib links the reactor); a thin subprocess plugin
// uses `serve.rs`, not these macros, so the re-export is gated to match.
#[cfg(feature = "in-process")]
pub use crate::{export_storage_plugin, export_tool_plugin};

// ── Deploy-lifecycle helpers ────────────────────────────────────────────
// `lifecycle::{run, stdout_string, timestamp}` — the exec/stderr/backup-stamp
// boilerplate every `*.install` / `*.backup` tool surface shared. Reached as
// `lifecycle::run(&mut cmd)`. Runs on the shared reactor, so it is always
// available — no reactor feature to gate on.
pub use crate::lifecycle;

// ── Struct derives ─────────────────────────────────────────────────────
// Plugin structs use `#[plugin_struct]` / `#[plugin_struct(args)]` (above)
// — it injects Serialize/Deserialize/JsonSchema/clap::Args anchored at
// `::plugin_toolkit::*`, so a plugin never names `serde`, `schemars`, or
// `clap`. The bare derive aliases below remain for the rare hand-rolled
// impl, but new code should prefer `#[plugin_struct]`.
pub use clap;
pub use schemars::JsonSchema;
pub use serde::{Deserialize, Serialize};

// ── JSON literal macro (orca-branded; serde_json is swappable) ──────────
// `json!({...})` lets plugins build ad-hoc JSON (e.g. test fixtures) without
// naming `serde_json`. Real payloads must be typed `#[plugin_struct]`s — the
// workspace bans opaque dynamic JSON values in source.
pub use serde_json::json;

// ── anyhow result + bail/anyhow macros ─────────────────────────────────
pub use anyhow::{Context, Result, anyhow, bail};

// ── async traits ───────────────────────────────────────────────────────
// `#[orca_async]` is orca's native sugar for async traits: implement a core
// domain trait (StorageBackend / RuntimeAdapter / notifications Backend) by
// writing plain `async fn` under this attribute. `async_trait` remains only for
// hand-written async tool thunks that still use it.
pub use async_trait::async_trait;
pub use derive::orca_async;
pub use thiserror;
pub use tracing;

// ── Toolkit runtime helpers ────────────────────────────────────────────
pub use crate::hash::sha256_hex;
// `runtime` houses the SQLite endpoint helpers `endpoint_resource!` emits;
// gated with the `db` feature so storage-only plugins drop rusqlite.
#[cfg(feature = "db")]
pub use crate::runtime;

// ── Endpoint addressing + per-instance connection fallback ─────────────
// `addresses` is a built-in column on every `endpoint_resource!` endpoint;
// `Address` is the row element and `resolve_reachable` is the fallback
// resolver plugins call to pick a live base URL at request time.
pub use crate::address::{self, Address};

// ── Ecosystem transport primitives ─────────────────────────────────────
// Plugins reach HTTP / GraphQL / OpenAPI through the toolkit so transport
// bug fixes land once and propagate. After `use plugin_toolkit::prelude::*;`
// these are in scope as `http::Client`, `graphql::Client`, `openapi::parse_str`,
// etc. — never `utils::http::…` or `::graphql::…` directly.
#[cfg(feature = "http")]
pub use crate::api_client::ApiClientBuilder;
#[cfg(feature = "graphql")]
pub use crate::graphql;
#[cfg(feature = "openapi")]
pub use crate::openapi;
#[cfg(feature = "http")]
pub use crate::{api_client, http};
