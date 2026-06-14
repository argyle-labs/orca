//! Macro-emission target-path consolidator.
//!
//! `derive`'s proc macros (`#[orca_tool]`, `endpoint_resource!`,
//! `#[derive(Replicated)]`) emit absolute paths like
//! `::orca_macro_runtime::anyhow::Result` and
//! `::orca_macro_runtime::db_types::SchemaFragment`. A consumer crate
//! only needs ONE dependency — this crate — for those paths to resolve.
//!
//! Domain (per [[feedback-no-re-export-layers]] carve-out): path
//! resolution for derive-macro emissions, and nothing else. Does NOT
//! provide source-use convenience — for that, see
//! `orca-plugin-toolkit::prelude`. Does NOT own type definitions — for
//! that, see `db-types`.
//!
//! Pairs with [[project-orca-macro-runtime-migration]]: this crate is
//! the WIRED end-state. Every derive emission goes through it; every
//! direct consumer of those derives depends on it.

// Workspace re-exports.
pub use contract;
pub use db_types;
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
