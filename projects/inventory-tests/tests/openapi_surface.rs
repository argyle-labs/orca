//! Shape guards on the generated OpenAPI surface.
//!
//! These live here rather than in `dispatch` because `dispatch`'s own test
//! binary links no buckets: `inventory::iter::<OpenApiToolRegistration>` is
//! EMPTY there and every assertion over it passes vacuously. This crate links
//! every bucket, so the walks below see the real tool surface.

#![allow(clippy::disallowed_types)] // walks the emitter's own dynamic JSON output

use std::collections::{BTreeMap, BTreeSet};

use dispatch::openapi::OpenApiToolRegistration;
use serde_json::Value;

// Side-effect imports — link the buckets so their inventory::submit! statics
// are pulled into this test binary. This list must cover every crate that
// hosts an #[orca_tool]; anything missing is silently unguarded.
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
// The server crate's lib target is named `orca`.
use orca as _;
use orca_inventory as _;
use plugin_toolkit as _;
use plugins as _;
use spec as _;
use system as _;

/// Every schema in the surface, paired with a human-readable origin so a
/// failure names the tool and side (args/output) that carries the offender.
fn all_schemas() -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for entry in inventory::iter::<OpenApiToolRegistration> {
        out.push((format!("{} args", entry.name), (entry.args_schema)()));
        out.push((format!("{} output", entry.name), (entry.output_schema)()));
    }
    out
}

/// Walk every `properties` map in a schema tree, invoking `f(property_name)`.
fn walk_properties(v: &Value, f: &mut impl FnMut(&str)) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Object(props)) = map.get("properties") {
                for key in props.keys() {
                    f(key);
                }
            }
            for child in map.values() {
                walk_properties(child, f);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk_properties(child, f);
            }
        }
        _ => {}
    }
}

/// Schemas (`"<tool> args"` / `"<tool> output"`) whose wire form stays
/// snake_case for one release, each with its reason. Drop an entry once the
/// fleet and plugins are past the camelCase rename.
const SNAKE_CASE_ORIGINS: &[(&str, &str)] = &[
    (
        "system.update args",
        "forwarded/fan-out legs reach previous-release peers, which drop camelCase keys",
    ),
    (
        "system.update output",
        "a previous-release controller decodes camelCase as all-default",
    ),
    (
        "media.unit.rescan output",
        "carries `RescanTarget`, sent daemon -> plugin; older media plugins drop camelCase keys",
    ),
    (
        "service.create output",
        "carries `BackupArtifact`, sent daemon -> plugin on restore; older service plugins drop camelCase keys",
    ),
    (
        "system.serve_release output",
        "a previous-release peer's upgrade path requires `asset_b64`",
    ),
];

/// The whole tool surface is camelCase on the wire. A property name holding an
/// underscore means some type on the path lost its
/// `#[serde(rename_all = "camelCase")]`, which silently splits the API's
/// naming contract in two.
#[test]
fn no_snake_case_property_names_on_the_tool_surface() {
    let mut offenders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (origin, schema) in all_schemas() {
        walk_properties(&schema, &mut |name| {
            if name.contains('_') && !SNAKE_CASE_ORIGINS.iter().any(|(o, _)| *o == origin) {
                offenders
                    .entry(name.to_string())
                    .or_default()
                    .insert(origin.clone());
            }
        });
    }
    assert!(
        offenders.is_empty(),
        "snake_case property names on the tool surface ({} distinct):\n{}",
        offenders.len(),
        offenders
            .iter()
            .map(|(name, origins)| format!(
                "  {name}  <- {}",
                origins.iter().cloned().collect::<Vec<_>>().join(", ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The surface must be non-empty, or the guard above passes vacuously — the
/// exact failure mode that let the snake_case drift accumulate unnoticed.
#[test]
fn the_openapi_surface_is_not_empty() {
    let names: Vec<&str> = inventory::iter::<OpenApiToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.len() > 100, "only {} tools registered", names.len());
    // Spot-check a tool from a crate that only this test links, so a dropped
    // side-effect import above fails loudly instead of shrinking the walk.
    assert!(names.iter().any(|n| n.starts_with("spec.")), "{names:?}");
    assert!(
        names.contains(&"spec.detail"),
        "server crate not linked: {names:?}"
    );
}

/// Two Rust types with the same ident collapse onto one `$defs` key, and the
/// spec can carry only one body — so the other type would be published with the
/// wrong schema. Every schema name must belong to exactly one type body.
#[test]
fn no_duplicate_schema_names_in_the_spec() {
    let collisions = dispatch::openapi::colliding_schema_names();
    assert!(
        collisions.is_empty(),
        "two different types claim each of these schema names: {collisions:?}"
    );
}

/// A tool type must not share a schema name with a utoipa-registered
/// component: the merge keeps the utoipa body, so the tool would be published
/// with the other type's schema.
#[test]
fn no_tool_schema_name_clashes_with_a_registered_component() {
    let spec = orca::serve::openapi::orca_spec_json();
    let components = spec["components"]["schemas"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    assert!(
        !components.is_empty(),
        "the built spec has no components — the walk would be vacuous"
    );
    let collisions = dispatch::openapi::colliding_schema_names_with(&components);
    assert!(
        collisions.is_empty(),
        "tool schema names clash with registered components: {collisions:?}"
    );
}
