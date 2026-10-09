//! Every id-named tool argument is a UUID (#783).
//!
//! Walks the input schema of every tool on the real `tools/list` surface and
//! requires each property named `id`, `*_id` or `*Id` to be declared
//! `format: uuid` — the schema `utils::id::Id` emits, and which its parser
//! enforces on every surface. A plain `String` id would let a hostname or
//! display name through to a verb that then resolves it.

#![allow(clippy::disallowed_types)] // walks the registry's own dynamic JSON schemas

use std::collections::BTreeSet;

use serde_json::Value;

// Side-effect imports — link every bucket that hosts an #[orca_tool] so the walk
// sees the whole surface; a crate missing here is silently unguarded.
use agents as _;
use auth as _;
use config_source as _;
use containers as _;
use conversation as _;
use db as _;
use dev as _;
use files as _;
use mcp as _;
use model as _;
use namespace as _;
use notifications as _;
use orca as _;
use orca_inventory as _;
use plugin_toolkit as _;
use plugins as _;
use spec as _;
use system as _;

/// `(tool, property, owner)` for ids that are natural keys minted outside orca's
/// UUID space, so they can never be UUIDs. The third column names who owns the
/// key's shape.
const NATURAL_KEY_IDS: &[(&str, &str, &str)] = &[
    (
        "backup.restore",
        "id",
        "backup store slot name `<timestamp>-<host>`, or `latest`",
    ),
    (
        "container.create",
        "id",
        "container runtime: docker id/name, LXC vmid",
    ),
    (
        "container.detail",
        "id",
        "container runtime: docker id/name, LXC vmid",
    ),
    (
        "container.update",
        "containerId",
        "container runtime: docker id, LXC vmid",
    ),
    ("guest.exec", "id", "Proxmox vmid"),
    ("guest.write_file", "id", "Proxmox vmid"),
    (
        "media.unit.detail",
        "id",
        "external `<source>:<id>` (imdb, asin, …)",
    ),
    (
        "media.unit.rescan",
        "id",
        "external `<source>:<id>` (tvdb, …)",
    ),
    ("model.create", "id", "operator-chosen model name"),
    ("model.delete", "id", "operator-chosen model name"),
    ("model.detail", "id", "operator-chosen model name"),
    ("model.update", "id", "operator-chosen model name"),
    (
        "diagnostics.repair",
        "repair_id",
        "provider-declared repair slug",
    ),
    ("notify.create", "repairId", "provider-declared repair slug"),
    ("pki.create", "pluginId", "plugin manifest id"),
    ("plugin.data.create", "instanceId", "plugin manifest id"),
    ("plugin.data.delete", "id", "plugin manifest id"),
    ("plugin.data.detail", "id", "plugin manifest id"),
    ("plugin.data.update", "id", "plugin manifest id"),
    (
        "storage.detail",
        "id",
        "storage backend locator (`s3://bucket/prefix`, …)",
    ),
    ("ups.config", "id", "UPS provider's device name"),
    ("ups.configure", "id", "UPS provider's device name"),
    ("ups.state", "id", "UPS provider's device name"),
];

/// `(tool, property)` for system ids, which take `system::system_id::SystemId`
/// rather than `Id`; they are typed with it in their own change.
const UNTYPED_SYSTEM_IDS: &[(&str, &str)] = &[
    ("system.detail", "id"),
    ("system.health", "id"),
    ("system.mesh.delete", "peerId"),
    ("system.mesh.update", "peerId"),
    ("system.telemetry.list", "peerId"),
    ("system.update", "id"),
];

fn is_id_name(name: &str) -> bool {
    name == "id" || name.ends_with("_id") || name.ends_with("Id")
}

fn is_uuid_typed(prop: &Value) -> bool {
    prop.get("format").and_then(Value::as_str) == Some("uuid")
        || prop.get("items").is_some_and(is_uuid_typed)
}

/// Collect every id-named property that is not UUID-typed, anywhere in the
/// schema tree (nested arg structs and `$defs` included).
fn collect(tool: &str, node: &Value, out: &mut BTreeSet<(String, String)>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(props)) = map.get("properties") {
                for (name, prop) in props {
                    if is_id_name(name) && !is_uuid_typed(prop) {
                        out.insert((tool.to_string(), name.clone()));
                    }
                }
            }
            for child in map.values() {
                collect(tool, child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect(tool, child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn every_id_argument_is_a_uuid() {
    let defs = dispatch::mcp_definitions();
    assert!(
        defs.len() > 30,
        "expected the full tool inventory to be linked, got {} tools",
        defs.len()
    );

    let mut offenders = BTreeSet::new();
    for tool in &defs {
        let name = tool["name"].as_str().unwrap_or("<unnamed>");
        collect(name, &tool["inputSchema"], &mut offenders);
    }
    let allowed: BTreeSet<(String, String)> = NATURAL_KEY_IDS
        .iter()
        .map(|(t, p, _)| (t.to_string(), p.to_string()))
        .chain(
            UNTYPED_SYSTEM_IDS
                .iter()
                .map(|(t, p)| (t.to_string(), p.to_string())),
        )
        .collect();

    let untyped: Vec<_> = offenders.difference(&allowed).collect();
    assert!(
        untyped.is_empty(),
        "id-named arguments must be UUID-typed (`utils::id::Id`); \
         a natural key owned outside orca goes in NATURAL_KEY_IDS with its reason: {untyped:#?}"
    );
    let stale: Vec<_> = allowed.difference(&offenders).collect();
    assert!(
        stale.is_empty(),
        "NATURAL_KEY_IDS entries that are UUID-typed or gone — remove them: {stale:#?}"
    );
}
