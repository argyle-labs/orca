//! Cross-bucket inventory smoke tests. Links every #[orca_tool] bucket and
//! verifies the registry sees them all without panicking on duplicates.

use dispatch::ToolRegistration;

// Side-effect imports — link the buckets in so their inventory::submit!
// registrations are pulled into this test binary.
use agents as _;
use auth as _;
use docker as _;
use homeassistant as _;
use mcp as _;
use platform as _;
use plugins as _;
use proxmox as _;
use system as _;
use utils as _;

#[test]
fn host_tools_present_in_inventory_slice() {
    let names: Vec<&'static str> = inventory::iter::<ToolRegistration>
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert!(names.contains(&"system.host.detail"), "{names:?}");
    assert!(names.contains(&"system.host.set"), "{names:?}");
    assert!(names.contains(&"system.host.refresh"), "{names:?}");
}

#[test]
fn dispatch_names_includes_host_tools() {
    let names = dispatch::names();
    assert!(names.contains(&"system.host.detail"));
    assert!(names.contains(&"system.host.set"));
    assert!(names.contains(&"system.host.refresh"));
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
    let count = inventory::iter::<ToolRegistration>.into_iter().count();
    assert!(count >= 128, "expected >=128 tools, got {count}");
}
