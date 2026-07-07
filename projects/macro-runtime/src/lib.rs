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
pub use contract;
pub use derive;
pub use dispatch;

// Ecosystem re-exports.
pub use anyhow;
pub use async_trait;
pub use clap;
pub use inventory;
pub use rusqlite;
pub use schemars;
pub use serde;
pub use serde_json;
pub use tokio;

// ── Macro-target registration types ────────────────────────────────────
// Heterogeneous row types per replicated entity → free-form JSON at the
// bundle boundary is intentional.

use ::anyhow::Result;
use ::rusqlite::Connection;

/// A standalone `CREATE TABLE IF NOT EXISTS …` fragment registered by
/// `endpoint_resource!` and applied by `db::apply_fragments`.
pub struct SchemaFragment {
    pub name: &'static str,
    pub sql: &'static str,
}

::inventory::collect!(SchemaFragment);

/// One entry per `#[derive(Replicated)]` type.
pub struct ReplicatedRegistration {
    pub name: &'static str,
    pub export: fn(&Connection) -> Result<::serde_json::Value>,
    pub merge: fn(&Connection, ::serde_json::Value) -> Result<usize>,
}

::inventory::collect!(ReplicatedRegistration);
