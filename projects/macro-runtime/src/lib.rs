//! Macro-emission target-path consolidator AND owner of the macro-target
//! registration types (`SchemaFragment`, `ReplicatedRegistration`).
//!
//! `derive`'s proc macros emit absolute paths like
//! `::macro_runtime::anyhow::Result` and `::macro_runtime::SchemaFragment`.
//! A consumer crate only needs ONE dependency — this crate — for those
//! paths to resolve.
//!
//! Previously split as a separate `db-types` crate; folded here since
//! "macro emission targets live in one foundation crate" is the canonical
//! shape.

#![allow(clippy::disallowed_types)]

// Workspace re-exports.
/// Canonical unix-millis wall clock for generated replication code (lww stamp +
/// tombstone) — single-sourced from `utils::time` so the unit matches fleet-wide.
pub use ::utils::time::now_millis_since_epoch;
pub use contract;
pub use derive;
pub use dispatch;

// Ecosystem re-exports.
pub use anyhow;
pub use async_trait;
pub use clap;
pub use inventory;
#[cfg(feature = "replication")]
pub use rusqlite;
pub use schemars;
pub use serde;
pub use serde_json;
pub use tokio;

// ── Macro-target registration types ────────────────────────────────────
// Heterogeneous row types per replicated entity → free-form JSON at the
// bundle boundary is intentional.

/// A standalone `CREATE TABLE IF NOT EXISTS …` fragment registered by
/// `endpoint_resource!` and applied by `db::apply_fragments`. Dependency-light
/// (name + SQL only) so a plugin that declares tables via `endpoint_resource!`
/// links no rusqlite — the daemon applies the fragment against its connection.
pub struct SchemaFragment {
    pub name: &'static str,
    pub sql: &'static str,
}

::inventory::collect!(SchemaFragment);

/// One entry per `#[derive(Replicated)]` type. Gated behind `replication`
/// because its export/merge fns take a live `rusqlite::Connection` — a
/// core-only concern (the daemon owns the connection + drives mesh sync).
#[cfg(feature = "replication")]
pub struct ReplicatedRegistration {
    pub name: &'static str,
    pub export: fn(&::rusqlite::Connection) -> ::anyhow::Result<::serde_json::Value>,
    pub merge: fn(&::rusqlite::Connection, ::serde_json::Value) -> ::anyhow::Result<usize>,
}

#[cfg(feature = "replication")]
::inventory::collect!(ReplicatedRegistration);

// Generic column-list table replication behind `endpoint_resource!(… lww = …)`.
#[cfg(feature = "replication")]
pub mod replicate_table;
