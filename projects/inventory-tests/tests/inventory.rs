//! Cross-bucket inventory smoke tests. Links every #[orca_tool] bucket and
//! verifies the registry sees them all without panicking on duplicates.

use dispatch::ToolRegistration;

// Side-effect imports — link the buckets in so their inventory::submit!
// registrations are pulled into this test binary.
use agents as _;
use auth as _;
use files as _;
use notifications as _;
use plugins as _;
use pod as _;
use system as _;

#[test]
fn host_tools_present_in_inventory_slice() {
    // Post-consolidation: `system.host.*` was folded into `system.*` per the
    // one-tool-per-resource rule. Sanity-check the canonical surface instead.
    let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"system.detail"), "{names:?}");
    assert!(names.contains(&"system.update"), "{names:?}");
    // Fat host facts split out of the lean `system.detail` onto their own
    // on-demand, dotted-domain endpoints.
    assert!(names.contains(&"system.info.detail"), "{names:?}");
    assert!(names.contains(&"system.info.claims.list"), "{names:?}");
}

#[test]
fn dispatch_names_includes_host_tools() {
    let names = dispatch::names();
    assert!(names.contains(&"system.detail"));
    assert!(names.contains(&"system.update"));
}

#[test]
fn pod_tools_present_in_inventory_slice() {
    let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"pod.list"), "{names:?}");
    assert!(names.contains(&"pod.join"), "{names:?}");
    assert!(names.contains(&"pod.leave"), "{names:?}");
    assert!(names.contains(&"pod.kick"), "{names:?}");
    assert!(names.contains(&"pod.trust"), "{names:?}");
    assert!(names.contains(&"pod.ping"), "{names:?}");
    assert!(names.contains(&"pod.recover"), "{names:?}");
    assert!(names.contains(&"pod.forget"), "{names:?}");
}

#[test]
fn inventory_slice_has_full_migrated_set() {
    // Floor sized to the current consolidated surface (~82). Bump only when
    // a real surface expansion lands — this guards against accidental
    // wholesale loss of registrations, not against ongoing consolidation.
    let count = inventory::iter::<ToolRegistration>.into_iter().count();
    assert!(count >= 70, "expected >=70 tools, got {count}");
}

/// Both surface-name mangles — MCP (`dots→_`) and hey-api operationId
/// (`camelCase`, collapsing `.`/`_`/`-`) — are non-injective, so two distinct
/// canonical tool names can collapse onto one MCP name or one operationId and
/// silently clobber each other in the generated surface / SDK. Assert the map
/// is injective across the whole linked inventory; a collision fails the build.
#[test]
fn mangled_surface_names_are_unique() {
    use std::collections::HashMap;

    let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();

    let mut mcp_seen: HashMap<String, &'static str> = HashMap::new();
    let mut op_seen: HashMap<String, &'static str> = HashMap::new();
    for &name in &names {
        let mcp = dispatch::openapi::mcp_name(name);
        if let Some(prev) = mcp_seen.insert(mcp.clone(), name)
            && prev != name
        {
            panic!("MCP-name collision: {prev:?} and {name:?} both mangle to {mcp:?}");
        }
        let op = dispatch::openapi::operation_id_for(name);
        if let Some(prev) = op_seen.insert(op.clone(), name)
            && prev != name
        {
            panic!("operationId collision: {prev:?} and {name:?} both mangle to {op:?}");
        }
    }
}
