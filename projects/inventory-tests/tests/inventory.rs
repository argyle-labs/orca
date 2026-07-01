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
