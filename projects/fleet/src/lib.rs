//! Fleet domain — pod/mesh, system, host, lifecycle, meta.

pub mod host;
pub mod host_status;
pub mod lifecycle;
pub mod meta;
pub mod pod;
pub mod system;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;

// Inventory-slice smoke tests — exercised in one binary that links every
// bucket so the full `#[orca_tool]` graph is verifiable end-to-end.
#[cfg(all(test, feature = "native"))]
mod inventory_tests {
    use orca_tool::ToolRegistration;
    use orca_tool::native_register;

    // Side-effect imports — link the buckets in so their inventory::submit!
    // registrations are pulled into this test binary.
    use agents as _;
    use auth as _;
    use docker as _;
    use docs as _;
    use homeassistant as _;
    use infra as _;
    use mgmt as _;
    use platform as _;
    use plugins as _;
    use proxmox as _;

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
    fn native_register_enrolls_host_tools() {
        let mut reg = orca_tool::ToolRegistry::new();
        native_register(&mut reg);
        let names = reg.names();
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
        assert!(names.contains(&"system.peer.list"), "{names:?}");
        assert!(names.contains(&"system.peer.create"), "{names:?}");
    }

    #[test]
    fn inventory_slice_has_full_migrated_set() {
        let count = inventory::iter::<ToolRegistration>.into_iter().count();
        assert!(count >= 128, "expected >=128 tools, got {count}");
    }
}
