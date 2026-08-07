//! Cross-bucket inventory smoke tests. Links every #[orca_tool] bucket and
//! verifies the registry sees them all without panicking on duplicates.

use dispatch::ToolRegistration;

// Side-effect imports — link the buckets in so their inventory::submit!
// registrations are pulled into this test binary.
use agents as _;
use auth as _;
use files as _;
use notifications as _;
use orca_inventory as _;
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
    // Post-collapse: the pod surface is the six canonical verbs. The former
    // join/offer/accept, trust/sync/recover/cancel_offer/settings,
    // kick/leave/forget, snapshot/instances, certs/history, and
    // network.topology_view tools fold into these; pod.ping is removed.
    assert!(names.contains(&"pod.list"), "{names:?}");
    assert!(names.contains(&"pod.detail"), "{names:?}");
    assert!(names.contains(&"pod.create"), "{names:?}");
    assert!(names.contains(&"pod.update"), "{names:?}");
    assert!(names.contains(&"pod.delete"), "{names:?}");
    assert!(
        !names.contains(&"pod.ping"),
        "pod.ping should be removed: {names:?}"
    );
}

#[test]
fn storage_surface_is_collapsed() {
    let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();
    // Canonical collapsed surface: one top-level `storage`, plus the dotted
    // `storage.mount.*` / `storage.share.*` sub-resources.
    assert!(names.contains(&"storage.list"), "{names:?}");
    assert!(names.contains(&"storage.detail"), "{names:?}");
    assert!(names.contains(&"storage.mount.create"), "{names:?}");
    assert!(names.contains(&"storage.mount.update"), "{names:?}");
    assert!(names.contains(&"storage.share.list"), "{names:?}");
    assert!(names.contains(&"storage.share.create"), "{names:?}");
    // Retired: the imperative one-offs and the legacy `storage_mount.*` /
    // `storage.shares` / `storage.usage` surfaces fold into the above.
    for gone in [
        "storage.shares",
        "storage.usage",
        "storage.mount",
        "storage.unmount",
        "storage.recover",
        "storage_mount.list",
        "storage_mount.create",
        "storage_share.list",
        "mount.list",
        "mount.create",
    ] {
        assert!(!names.contains(&gone), "`{gone}` should be gone: {names:?}");
    }
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
