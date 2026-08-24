//! End-to-end loader coverage against a *real* subprocess plugin.
//! (self-exec: the test binary plays both the plugin and the driver.)
#![cfg(unix)]

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

const DOMAIN_ENV: &str = "LOADER_IT_DOMAIN";
const PLUGIN_ID: &str = "loader-it-plugin";
const TOOL: &str = "loader_it.echo";

fn run_as_plugin(sock: String) -> ! {
    let domain = std::env::var(DOMAIN_ENV).unwrap_or_else(|_| "topology".to_string());
    let stream = UnixStream::connect(&sock).expect("plugin: connect session socket");
    let hello = plugin_proto::Frame::Hello {
        protocol: plugin_proto::PROTOCOL_VERSION.to_string(),
        plugin: PLUGIN_ID.to_string(),
        version: "9.9.9".to_string(),
        manifest: vec![plugin_proto::ToolDef {
            name: TOOL.to_string(),
            description: "echoes its args".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
        }],
        backends: vec![serde_json::json!({
            "domain": domain, "name": "loader-it-backend", "invoke_prefix": "loader_it",
        })],
        schema: serde_json::Value::Null,
    };
    let served = plugin_proto::serve(stream, hello, |tool, args, _caps| {
        if tool == TOOL {
            Ok(args)
        } else {
            Err(format!("no such tool: {tool}"))
        }
    });
    let _keep = served.is_ok();
    std::process::exit(0);
}

fn self_exe() -> PathBuf {
    std::env::current_exe().expect("current_exe")
}
fn check(cond: bool, msg: &str) {
    if !cond {
        panic!("assertion failed: {msg}");
    }
}

fn main() {
    if let Ok(sock) = std::env::var("ORCA_PLUGIN_SOCKET") {
        run_as_plugin(sock);
    }
    unsafe { std::env::set_var("TMPDIR", "/tmp") };
    // Phase 1: unknown-domain backend refused + rollback.
    unsafe { std::env::set_var(DOMAIN_ENV, "loader-it-nonexistent-domain") };
    let bad = plugin_loader::spawn_plugin(&self_exe(), Some(PLUGIN_ID));
    check(bad.is_err(), "unknown domain must fail the load");
    let bad_err = format!("{:#}", bad.unwrap_err());
    check(
        bad_err.contains("unknown domain"),
        &format!("names domain: {bad_err}"),
    );
    check(
        !plugin_loader::is_loaded(PLUGIN_ID),
        "refused load not marked loaded",
    );
    // Phase 2: valid load + invoke surface.
    unsafe { std::env::set_var(DOMAIN_ENV, "topology") };
    let report = plugin_loader::spawn_plugin(&self_exe(), Some(PLUGIN_ID)).expect("valid load");
    check(
        report.software == PLUGIN_ID && report.semver == "9.9.9",
        "report header",
    );
    check(
        report.tools.contains(&TOOL.to_string()),
        "report lists tool",
    );
    check(plugin_loader::is_loaded(PLUGIN_ID), "plugin loaded");
    check(
        plugin_loader::loaded_plugins().iter().any(|p| {
            p.software == PLUGIN_ID && p.semver == "9.9.9" && p.tools.contains(&TOOL.to_string())
        }),
        "loaded_plugins",
    );
    check(
        plugin_loader::loaded_tool_defs()
            .iter()
            .any(|t| t.name == TOOL),
        "tool_defs",
    );
    let out = plugin_loader::invoke_plugin(TOOL, &serde_json::json!({"n": 7}))
        .expect("owns tool")
        .expect("echo ok");
    check(out == serde_json::json!({"n": 7}), "echo verbatim");
    check(
        plugin_loader::invoke_plugin("loader_it.not_a_tool", &serde_json::json!({})).is_none(),
        "unowned tool None",
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("rt");
    let cfg = Arc::new(contract::config::Config::load().expect("config"));
    let ctx = contract::ToolCtx::new(cfg);
    let dispatched = rt
        .block_on(plugin_loader::dispatch(
            TOOL,
            serde_json::json!({"k": "v"}),
            &ctx,
        ))
        .expect("async dispatch");
    check(
        dispatched == serde_json::json!({"k": "v"}),
        "async dispatch echo",
    );
    // Phase 3: reload replaces in place.
    let report2 = plugin_loader::spawn_plugin(&self_exe(), Some(PLUGIN_ID)).expect("reload");
    check(report2.software == PLUGIN_ID, "reload software");
    check(
        plugin_loader::loaded_plugins()
            .iter()
            .filter(|p| p.software == PLUGIN_ID)
            .count()
            == 1,
        "one copy after reload",
    );
    // Phase 4: unload reverses routes + backend.
    check(
        plugin_loader::unload_plugin(PLUGIN_ID) == 1,
        "unload removes one",
    );
    check(!plugin_loader::is_loaded(PLUGIN_ID), "gone after unload");
    check(
        plugin_loader::invoke_plugin(TOOL, &serde_json::json!({})).is_none(),
        "route freed",
    );
    check(
        plugin_loader::unload_plugin(PLUGIN_ID) == 0,
        "second unload no-op",
    );
    println!("subprocess_plugin: all phases passed");
}
