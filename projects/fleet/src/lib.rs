//! Fleet domain — pod/mesh, host, meta. (lifecycle dissolved into `system` crate, slice B3.)

#[cfg(feature = "native")]
pub mod cli;
pub mod host;
#[cfg(feature = "native")]
pub mod host_identity;
pub mod host_status;
#[cfg(feature = "native")]
pub mod host_status_writer;
#[cfg(feature = "native")]
pub mod infra;
pub mod meta;
#[cfg(feature = "native")]
pub mod native_support;
pub mod pod;
#[cfg(feature = "native")]
pub mod pod_native;

#[cfg(all(test, feature = "native"))]
pub(crate) mod test_support;

// Inventory-slice smoke tests — exercised in one binary that links every
// bucket so the full `#[orca_tool]` graph is verifiable end-to-end.
#[cfg(all(test, feature = "native"))]
mod inventory_tests {
    use orca_dispatch::ToolRegistration;

    // Side-effect imports — link the buckets in so their inventory::submit!
    // registrations are pulled into this test binary.
    use agents as _;
    use auth as _;
    use docker as _;
    use docs as _;
    use homeassistant as _;
    use mcp as _;
    use platform as _;
    use plugins as _;
    use proxmox as _;
    use system as _;

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
        let names = orca_dispatch::names();
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
