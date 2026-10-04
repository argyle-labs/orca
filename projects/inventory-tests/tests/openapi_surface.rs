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
        "plugin.serve_asset args",
        "delegated plugin fetch reaches previous-release peers, which drop camelCase keys",
    ),
    (
        "plugin.serve_asset output",
        "a previous-release peer's delegated plugin fetch requires `asset_b64`",
    ),
    (
        "system.serve_release output",
        "a previous-release peer's upgrade path requires `asset_b64`",
    ),
    (
        "system.list output",
        "previous-release roster sync decodes it; a failed decode marks new peers down",
    ),
    (
        "system.detail output",
        "a previous-release peer-detail cache decodes `SystemStatusReport`",
    ),
    (
        "system.mesh.update output",
        "a previous-release `push_trust` decodes `MeshTrustOutput`",
    ),
    (
        "system.health output",
        "carries `DaemonRuntimeStatus`, shared with `system.detail`",
    ),
    (
        "system.topology output",
        "carries `TopologyFacts`, `VersionEntry` and `TopologyClaim`, shared with `system.list`",
    ),
    (
        "system.info.detail output",
        "carries `TopologyClaim`, shared with `system.list`",
    ),
    (
        "system.info.claims.list output",
        "carries `TopologyClaim`, shared with `system.list`",
    ),
    (
        "system.telemetry.list output",
        "carries `TopologyClaim`, shared with `system.list`",
    ),
];

/// An allowlist entry that names no schema is stale and would silently excuse
/// a future tool that reuses the name.
#[test]
fn every_snake_case_origin_names_a_real_schema() {
    let origins: Vec<String> = all_schemas().into_iter().map(|(o, _)| o).collect();
    for (origin, _why) in SNAKE_CASE_ORIGINS {
        assert!(
            origins.iter().any(|o| o == origin),
            "SNAKE_CASE_ORIGINS entry `{origin}` matches no tool schema — drop it"
        );
    }
}

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

fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Property names, per tool, of the rc.11 tool surface: `side` is `"args"` or
/// `"outputs"`. rc.11 is the last release whose daemons read and write
/// snake_case only.
fn rc11_properties(side: &str) -> BTreeMap<String, BTreeSet<String>> {
    let all: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> =
        serde_json::from_str(include_str!("fixtures/rc11_properties.json"))
            .expect("parse rc.11 fixture");
    all.get(side).cloned().unwrap_or_default()
}

fn camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for c in name.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Every property whose rc.11 spelling (snake_case or camelCase) this build no
/// longer emits, per tool, under its current name. Tools absent from rc.11 are
/// skipped: an rc.11 daemon refuses them as unknown, so nothing is silently
/// dropped.
fn renamed_since_rc11(
    side: &str,
    schema: fn(&OpenApiToolRegistration) -> Value,
) -> BTreeMap<String, BTreeSet<String>> {
    let rc11 = rc11_properties(side);
    let mut renamed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in inventory::iter::<OpenApiToolRegistration> {
        let Some(old) = rc11.get(entry.name) else {
            continue;
        };
        walk_properties(&schema(entry), &mut |name| {
            if old.contains(name) {
                return;
            }
            let (snake, camel) = (snake_case(name), camel_case(name));
            if (snake != name && old.contains(&snake)) || (camel != name && old.contains(&camel)) {
                renamed
                    .entry(entry.name.to_string())
                    .or_default()
                    .insert(name.to_string());
            }
        });
    }
    renamed
}

/// `wire_compat::RENAMED_ARGS` gates forwarding to peers that predate the
/// camelCase wire. It must name exactly the args renamed since rc.11: a
/// missing entry lets an old peer silently drop a field; an extra one refuses
/// calls that were already camelCase there.
#[test]
fn wire_compat_renamed_args_match_the_rc11_schema() {
    let expected = renamed_since_rc11("args", |e| (e.args_schema)());
    let actual: BTreeMap<String, BTreeSet<String>> = system::mesh::wire_compat::RENAMED_ARGS
        .iter()
        .map(|(tool, keys)| {
            (
                tool.to_string(),
                keys.iter().map(|k| k.to_string()).collect(),
            )
        })
        .collect();
    assert!(
        expected == actual,
        "RENAMED_ARGS drifted from the rc.11 schema. Expected:\n{}",
        expected
            .iter()
            .map(|(tool, keys)| format!(
                "    (\"{tool}\", &[{}]),",
                keys.iter()
                    .map(|k| format!("\"{k}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Outputs an rc.11 daemon decodes from an upgraded peer on its own, with no
/// operator involved. Each must keep every rc.11 property name, or the old
/// daemon misreads its peers.
const RC11_DECODED_OUTPUTS: &[(&str, &str)] = &[
    (
        "system.list",
        "roster sync decodes it every 60s; a failed decode marks new peers down",
    ),
    (
        "system.detail",
        "the peer-detail cache decodes `SystemStatusReport` for roster rows",
    ),
    (
        "system.mesh.update",
        "`push_trust` decodes the peer's `MeshTrustOutput`; a failure half-applies trust",
    ),
    (
        "system.update",
        "fleet rolls and the update probe decode the peer's update state",
    ),
    (
        "system.health",
        "the fleet health sweep decodes each peer's report",
    ),
];

/// Outputs renamed since rc.11 that only an operator's `--id` call from an
/// rc.11 CLI decodes. That old controller drops the renamed keys, and fails
/// outright where one is required; the remedy is to run the CLI from an
/// upgraded host. Listed so a new rename is a decision, not an accident.
const RC11_CONTROLLER_LIMITS: &[&str] = &[
    "auth.login",
    "auth.token.list",
    "config.detail",
    "config.list",
    "config.source.diff",
    "config.source.status",
    "config.upsert",
    "container.create",
    "container.list",
    "container.update",
    "files.list",
    "guest.exec",
    "media.detail",
    "media.list",
    "media.unit.detail",
    "media.unit.list",
    "model.backends_check",
    "model.list",
    "namespace.access.list",
    "namespace.create",
    "namespace.detail",
    "namespace.list",
    "pki.list",
    "schedule.create",
    "schedule.detail",
    "schedule.list",
    "schema.detail",
    "schema.list",
    "secrets.list",
    "service.list",
    "spec.list",
    "storage.detail",
    "storage.mount.create",
    "storage.mount.detail",
    "storage.mount.list",
    "storage.mount.update",
    "storage.share.repair-permissions",
    "system.build",
    "system.certs.list",
    "system.history",
    "system.info.detail",
    "system.join",
    "system.mesh.delete",
    "system.telemetry.list",
    "system.topology",
    "system.uninstall",
    "web.update",
];

#[test]
fn rc11_decoded_outputs_keep_their_rc11_names() {
    let renamed = renamed_since_rc11("outputs", |e| (e.output_schema)());
    let broken: Vec<String> = RC11_DECODED_OUTPUTS
        .iter()
        .filter_map(|(tool, why)| {
            renamed
                .get(*tool)
                .map(|keys| format!("  {tool} ({why}): {keys:?}"))
        })
        .collect();
    assert!(
        broken.is_empty(),
        "outputs an rc.11 daemon decodes were renamed:\n{}",
        broken.join("\n")
    );
}

#[test]
fn every_output_renamed_since_rc11_is_an_accepted_controller_limit() {
    let renamed = renamed_since_rc11("outputs", |e| (e.output_schema)());
    let unlisted: Vec<&String> = renamed
        .keys()
        .filter(|tool| !RC11_CONTROLLER_LIMITS.contains(&tool.as_str()))
        .filter(|tool| !RC11_DECODED_OUTPUTS.iter().any(|(t, _)| t == tool))
        .collect();
    let stale: Vec<&&str> = RC11_CONTROLLER_LIMITS
        .iter()
        .filter(|tool| !renamed.contains_key(**tool))
        .collect();
    assert!(
        unlisted.is_empty() && stale.is_empty(),
        "unlisted renamed outputs: {unlisted:?}; stale RC11_CONTROLLER_LIMITS entries: {stale:?}"
    );
}
