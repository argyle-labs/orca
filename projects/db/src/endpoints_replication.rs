//! Mesh replication for the shared, core-migrated `endpoints` table.
//!
//! Thin `endpoint_resource!` plugins (proxmox, docker, ntfy, …) all write
//! PROVIDER-TAGGED rows into the ONE `endpoints` table. Because the table is
//! shared, its replication is registered ONCE here in core — not per plugin —
//! keyed by the minted `id` PK with last-write-wins on `updated_at`. This uses
//! the same generic column-list replicator (`macro_runtime::replicate_table`)
//! the per-plugin `endpoint_resource!(… lww = …)` tables use.

#![allow(clippy::disallowed_types)]

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

/// Column list mirrors the `endpoints` schema (see `apply_schema`) exactly.
const COLUMNS: &[&str] = &[
    "id",
    "provider",
    "name",
    "routes",
    "enabled",
    "auth_principal",
    "insecure",
    "created_at",
    "updated_at",
];

fn export(conn: &Connection) -> Result<Value> {
    macro_runtime::replicate_table::export_table(conn, "endpoints", COLUMNS, "id")
}

fn merge(conn: &Connection, rows: Value) -> Result<usize> {
    macro_runtime::replicate_table::merge_table(
        conn,
        "endpoints",
        COLUMNS,
        "id",
        "updated_at",
        rows,
    )
}

inventory::submit! {
    macro_runtime::ReplicatedRegistration {
        name: "endpoints",
        export,
        merge,
    }
}
