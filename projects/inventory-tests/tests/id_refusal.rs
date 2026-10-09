//! Every UUID-typed id argument refuses a non-id at dispatch (#783).
//!
//! The schema walk in `id_args.rs` is only a promise; this proves the parse
//! behind it. Kept as the only test in its binary because it points the
//! process env at a throwaway home.

#![allow(clippy::disallowed_types)] // walks the registry's own dynamic JSON schemas

use inventory_tests::{is_id_name, is_uuid_typed};
use serde_json::Value;

// Side-effect imports — link every bucket that hosts an #[orca_tool] so the walk
// sees the whole surface; a crate missing here is silently unguarded.
use agents as _;
use auth as _;
use config_source as _;
use containers as _;
use conversation as _;
use db as _;
use dev as _;
use files as _;
use mcp as _;
use model as _;
use namespace as _;
use notifications as _;
use orca as _;
use orca_inventory as _;
use plugin_toolkit as _;
use plugins as _;
use spec as _;
use system as _;

/// Top-level UUID-typed id properties of every registered tool.
fn uuid_id_props() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for tool in dispatch::mcp_definitions() {
        let name = tool["name"].as_str().unwrap_or_default().to_string();
        if let Some(Value::Object(props)) = tool["inputSchema"].get("properties") {
            for (prop, schema) in props {
                if is_id_name(prop) && is_uuid_typed(schema) {
                    out.push((name.clone(), prop.clone()));
                }
            }
        }
    }
    out
}

/// A hostname, or a blank that could pass for "omitted", sent as an id must be
/// refused by every tool before its body runs — dry-run gate included — on the
/// same dispatch path REST and MCP take, with the error naming the argument.
#[tokio::test]
async fn a_hostname_or_blank_sent_as_an_id_is_refused() {
    // Config::load reads the home dir; it must be a throwaway, never the
    // operator's.
    let home = tempfile::tempdir().expect("tempdir");
    // SAFETY: the only test in this binary, and set before any thread reads
    // the env.
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
        std::env::set_var("HOME", home.path());
        std::env::set_var("ORCA_DB_PATH", home.path().join("orca.db"));
    }
    let cfg = std::sync::Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);

    let props = uuid_id_props();
    assert!(
        props.iter().any(|(t, p)| t == "system.update" && p == "id"),
        "system.update --id must be UUID-typed: {props:?}"
    );
    let mut accepted = Vec::new();
    for (tool, prop) in &props {
        for bad in ["orca-id-probe-host", "", "   "] {
            let mut args = serde_json::Map::new();
            args.insert(prop.clone(), Value::String(bad.into()));
            match dispatch::dispatch(tool, Value::Object(args), &ctx).await {
                Err(e)
                    if {
                        let msg = format!("{e:#}");
                        msg.contains("UUID") && msg.contains(&format!("{prop}: "))
                    } => {}
                other => accepted.push(format!("{tool}.{prop}={bad:?}: {other:?}")),
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "these tools did not refuse a non-id, naming the argument: {accepted:#?}"
    );
}
