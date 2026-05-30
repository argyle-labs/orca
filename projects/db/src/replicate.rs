//! Generic shared-state replication registry.
//!
//! A row type opts into mesh replication with `#[derive(Replicated)]`
//! (see `orca-macro`):
//!
//! ```ignore
//! #[derive(Serialize, Deserialize, Replicated)]
//! #[replicate(table = "users", lww = "updated_at")]
//! pub struct ReplicaUser { pub id: String, /* … */ pub updated_at: String }
//! ```
//!
//! The derive emits one [`ReplicatedRegistration`] into the inventory slice
//! this crate collects, wiring two type-erased fns:
//!   - **export** — `SELECT` every row of `table`, as a JSON array;
//!   - **merge** — upsert each incoming row last-write-wins on the `lww` column,
//!     keyed by the primary key (`pk`, default `id`).
//!
//! The pod mesh engine walks [`registrations`] to build ONE signed bundle
//! (`{ entity_name -> rows }`) per peer rather than a bespoke method per
//! entity. `users` is the first registrant; configs/settings follow.
//!
//! This crate is deliberately tiny and DB-flavoured (it speaks
//! `rusqlite::Connection`) but transport-agnostic — signing, the mTLS dial,
//! and the periodic schedule all live in the pod crate.

// This crate is a registry of *heterogeneous* entity rows — each entity has a
// different typed row, so the common bundle boundary is genuinely free-form
// JSON. The concrete typing happens inside each entity's generated export/merge.
#![allow(clippy::disallowed_types)]

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

/// One entry per `#[derive(Replicated)]` type. `export`/`merge` are generated
/// to operate on the type's backing table; the engine never needs to know the
/// concrete row type.
pub struct ReplicatedRegistration {
    /// Entity name — the backing table, used as the bundle key + log label.
    pub name: &'static str,
    /// Serialize every local row of the entity to a JSON array.
    pub export: fn(&Connection) -> Result<Value>,
    /// Merge a JSON array of rows last-write-wins. Returns the number of local
    /// rows created or updated.
    pub merge: fn(&Connection, Value) -> Result<usize>,
}

inventory::collect!(ReplicatedRegistration);

/// Every registered entity, in stable name order.
pub fn registrations() -> Vec<&'static ReplicatedRegistration> {
    let mut v: Vec<_> = inventory::iter::<ReplicatedRegistration>().collect();
    v.sort_by_key(|r| r.name);
    v
}

/// Export every registered entity into a `{ name -> rows }` bundle.
pub fn export_all(conn: &Connection) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for reg in registrations() {
        out.insert(reg.name.to_string(), (reg.export)(conn)?);
    }
    Ok(out)
}

/// Merge an incoming bundle, dispatching each entity to its registered `merge`.
/// Unknown entity names are skipped (forward-compat with peers that replicate
/// entities this host doesn't know). Returns total rows created/updated.
pub fn merge_bundle(conn: &Connection, bundle: BTreeMap<String, Value>) -> Result<usize> {
    let mut total = 0;
    for reg in registrations() {
        if let Some(rows) = bundle.get(reg.name) {
            match (reg.merge)(conn, rows.clone()) {
                Ok(n) => total += n,
                Err(e) => tracing::warn!("[replicate] merge of '{}' failed: {e:#}", reg.name),
            }
        }
    }
    Ok(total)
}
