//! Inventory-collected schema fragments.
//!
//! Hand-written tables live in [`crate::apply_schema`]'s big
//! `execute_batch`. Toolkit-generated tables (`endpoint_resource!` and
//! friends) register a [`SchemaFragment`] via `inventory::submit!` so
//! they're picked up automatically at db-open time without anyone having
//! to edit `apply_schema`. See
//! [[feedback-plugin-toolkit-max-power-min-boilerplate]].

use anyhow::Result;
use db_types::SchemaFragment;
use rusqlite::Connection;

/// Apply every registered fragment. Idempotent — each fragment uses
/// `IF NOT EXISTS`. Errors surface with the fragment name so a typo in
/// the macro-emitted SQL points back at the offending plugin.
pub fn apply_fragments(conn: &Connection) -> Result<()> {
    for f in inventory::iter::<SchemaFragment> {
        conn.execute_batch(f.sql)
            .map_err(|e| anyhow::anyhow!("schema fragment `{}` failed to apply: {e}", f.name))?;
    }
    Ok(())
}
