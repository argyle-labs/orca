//! Proves `export_tool_plugin!` expands and the macro-emitted ABI fns behave:
//! `__manifest` filters to the plugin's namespace, `__backends`/`__schemas` are
//! empty, and `__invoke` rejects foreign tool names. (Tool execution itself is
//! the dispatch registry's own concern; here we pin the export wiring.)
#![allow(clippy::disallowed_types)]

use plugin_toolkit::abi_stable::std_types::{RErr, RStr};

plugin_toolkit::export_tool_plugin! {
    name: "exampletool",
    target_compat: ">=1.0",
}

#[test]
fn backends_and_schemas_are_empty_for_a_tool_plugin() {
    assert_eq!(__backends().as_str(), "[]");
    assert_eq!(__schemas().as_str(), r#"{"namespace":"","tables":[]}"#);
}

#[test]
fn manifest_is_valid_json_scoped_to_the_prefix() {
    // No `exampletool.*` tools are linked into this test crate, so the filtered
    // manifest is the empty array — but it must be well-formed and contain no
    // other plugin's tools.
    let m = __manifest();
    assert_eq!(m.as_str(), "[]");
}

#[test]
fn invoke_rejects_names_outside_the_namespace() {
    let r = __invoke(RStr::from("docker.ps"), RStr::from("{}"));
    assert!(matches!(r, RErr(ref e) if e.as_str().contains("exampletool.")));
}
