#![cfg(not(feature = "in-process"))]

#[test]
fn thin_plugin_build_leaves_dispatch_in_process_off() {
    assert!(!plugin_toolkit::dispatch::IN_PROCESS);
}
