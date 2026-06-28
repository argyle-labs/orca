//! The multi-app arm of `export_tool_plugin!` — one cdylib hosting several mesh
//! namespaces (the `arr` shape). Separate test crate because each export macro
//! emits one cdylib root module.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::abi_stable::std_types::{RErr, RStr};

plugin_toolkit::export_tool_plugin! {
    name: "arr",
    target_compat: "v3",
    tool_prefixes: ["sonarr.", "radarr.", "prowlarr.", "lidarr."],
}

#[test]
fn multi_manifest_is_valid_json_scoped_to_all_app_prefixes() {
    // No sonarr.*/radarr.* tools are linked into this test crate, so the
    // filtered manifest is empty — but well-formed and free of foreign tools.
    assert_eq!(__manifest().as_str(), "[]");
    assert_eq!(__backends().as_str(), "[]");
}

#[test]
fn multi_invoke_admits_each_app_prefix_and_rejects_others() {
    // Each hosted app's namespace is admitted (then dispatched — here the
    // foreign-to-this-crate name fails at dispatch, which is fine: admission is
    // what the arm controls). A name outside all prefixes is rejected up front
    // with the namespace list in the error.
    let r = __invoke(RStr::from("docker.ps"), RStr::from("{}"));
    match r {
        RErr(e) => {
            let e = e.as_str();
            assert!(
                e.contains("sonarr.") && e.contains("lidarr."),
                "namespace list expected: {e}"
            );
        }
        _ => panic!("foreign tool must be rejected"),
    }
}
