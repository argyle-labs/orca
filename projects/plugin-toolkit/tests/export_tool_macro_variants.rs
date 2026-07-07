//! The multi-app and hybrid arms of `export_tool_plugin!`. Each export macro
//! emits one cdylib root module, so the two variants live in separate test
//! crates — this file pins the **hybrid** arm; `export_tool_macro_multi.rs`
//! pins multi-app.
#![allow(clippy::disallowed_types)]

use plugin_toolkit::abi_stable::std_types::{RErr, ROk, RStr};

/// Stand-in for a plugin's notification-backend descriptor enumeration.
fn fake_backends_json() -> String {
    r#"[{"domain":"notifications","name":"ep1","kind":"","endpoint":"https://x","capabilities":["emit"],"invoke_prefix":"notify.__backend.ep1"}]"#.to_string()
}

/// Stand-in for a plugin's `*.__backend.*` handler: owns the notify backend
/// emit calls, returns None for everything else (→ tool dispatch).
fn fake_backend_dispatch(name: &str, _args: &str) -> Option<Result<String, String>> {
    if let Some(rest) = name.strip_prefix("notify.__backend.") {
        return Some(Ok(format!("{{\"emitted\":\"{rest}\"}}")));
    }
    None
}

plugin_toolkit::export_tool_plugin! {
    name: "hybridtest",
    target_compat: "1.0",
    backends: fake_backends_json(),
    backend_dispatch: fake_backend_dispatch,
}

#[test]
fn hybrid_backends_come_from_the_plugin_expr() {
    let j = __backends().into_string();
    assert!(j.contains(r#""domain":"notifications""#), "{j}");
    assert!(
        j.contains(r#""invoke_prefix":"notify.__backend.ep1""#),
        "{j}"
    );
}

#[test]
fn hybrid_invoke_routes_backend_calls_to_the_hook() {
    // A `*.__backend.*` call is handled by the plugin hook, not tool dispatch.
    let r = __invoke(
        RStr::from("notify.__backend.ep1.emit"),
        RStr::from(r#"{"msg":"hi"}"#),
    );
    match r {
        ROk(s) => assert!(
            s.as_str().contains(r#""emitted":"ep1.emit""#),
            "{}",
            s.as_str()
        ),
        RErr(e) => panic!("backend call should be handled: {}", e.as_str()),
    }
}

#[test]
fn hybrid_invoke_falls_through_to_tool_surface() {
    // A non-backend name the hook doesn't own falls through to tool dispatch,
    // which (no hybridtest.* tools linked here) rejects a foreign name.
    let r = __invoke(RStr::from("docker.ps"), RStr::from("{}"));
    assert!(matches!(r, RErr(_)));
}
