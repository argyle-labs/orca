//! Macro-emission target types for db-flavoured derive macros.
//!
//! This crate is the canonical home of `SchemaFragment` and
//! `ReplicatedRegistration`. The `derive` crate emits
//! `::db_types::SchemaFragment` (from `endpoint_resource!`) and
//! `::db_types::ReplicatedRegistration` (from `#[derive(Replicated)]`),
//! which means every crate that USES those macros depends on `db_types`
//! — including `db` itself.
//!
//! Sits BELOW `db` in the dep graph so the macro paths resolve without
//! creating a cycle (db is both a macro target AND a macro consumer).
//!
//! Consumer-side helpers (`apply_fragments`, `registrations`,
//! `export_all`, `merge_bundle`, `roots`, write-notify channel) stay in
//! `db` — they're orchestration over the inventory slices, not the
//! target types themselves.

// This crate's whole job is to host registry entries for *heterogeneous*
// row types — each replicated entity has a different typed row, so the
// common bundle boundary is genuinely free-form JSON, exactly as in the
// original `db::replicate` module. The concrete typing happens inside
// each entity's generated export/merge.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use rusqlite::Connection;

/// A standalone `CREATE TABLE IF NOT EXISTS …` (and optional indices)
/// for one resource table. Registered into inventory by
/// `endpoint_resource!`; applied by `db::apply_fragments` after the
/// hand-coded schema runs.
pub struct SchemaFragment {
    pub name: &'static str,
    pub sql: &'static str,
}

inventory::collect!(SchemaFragment);

/// One entry per `#[derive(Replicated)]` type. `export`/`merge` are
/// generated to operate on the type's backing table; the replication
/// engine never needs to know the concrete row type.
pub struct ReplicatedRegistration {
    /// Entity name — the backing table, used as the bundle key + log label.
    pub name: &'static str,
    /// Serialize every local row of the entity to a JSON array.
    pub export: fn(&Connection) -> Result<serde_json::Value>,
    /// Merge a JSON array of rows last-write-wins. Returns the number
    /// of local rows created or updated.
    pub merge: fn(&Connection, serde_json::Value) -> Result<usize>,
}

inventory::collect!(ReplicatedRegistration);
