//! Slice-4 integration proof: the `plugin.*` install surface on top of the
//! proven slice-3 cdylib loader.
//!
//! Exercises the *real* registered tools through `dispatch::dispatch` (the same
//! entrypoint REST/MCP/CLI use), against a temp `$ORCA_HOME` so nothing touches
//! the operator's real install dir. The cdylib-invocability seam is wired by
//! installing the same dynamic-dispatch fallback the daemon installs at startup.
//!
//! Four proofs, in one test (the plugin registry is process-global, so order
//! matters and a single test keeps it deterministic):
//!   (i)   `plugin.list` shows jellyfin in the embedded catalog.
//!   (ii)  `plugin.install --file <libjellyfin>` passes the gate, lands in the
//!         install dir, and `jellyfin.*` tools become invocable.
//!   (iii) `plugin.install --file <bogus>` is cleanly REFUSED and NOT installed.
//!   (iv)  the startup scan loads an already-installed plugin.
//!
//! Gated on `JELLYFIN_CDYLIB` (absolute path to `libjellyfin.dylib`). When unset
//! the test early-returns so CI without the artifact stays green.
//
// `dispatch::dispatch` is the type-erased tool seam: args + results cross it as
// opaque JSON values, exactly as REST/MCP do. A test driving that seam works in
// those values by necessity, so the workspace `disallowed_types` lint is
// suppressed here (per its own guidance for genuinely free-form boundary data).
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use contract::ToolCtx;
use serde_json::{Value, json};

fn ctx() -> ToolCtx {
    let config = contract::config::Config::load().expect("Config::load under temp ORCA_HOME");
    ToolCtx::new(Arc::new(config))
}

async fn call(name: &str, args: Value, ctx: &ToolCtx) -> anyhow::Result<Value> {
    dispatch::dispatch(name, args, ctx).await
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_surface_end_to_end() {
    let Ok(cdylib) = std::env::var("JELLYFIN_CDYLIB") else {
        eprintln!("JELLYFIN_CDYLIB unset — skipping plugin.* install-surface proof");
        return;
    };

    // Isolated state dir: Config::load + the install dir both resolve under it.
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: single-threaded test setup before any concurrent access.
    unsafe {
        std::env::set_var("ORCA_HOME", tmp.path());
        std::env::set_var("HOME", tmp.path());
    }
    let ctx = ctx();

    // Wire the cdylib fallback exactly as the daemon does, so plugin tools are
    // invocable through `dispatch::dispatch`.
    dispatch::set_dynamic_dispatch(
        Box::new(plugin_loader::invoke_plugin),
        Box::new(|| {
            plugin_loader::loaded_tool_defs()
                .iter()
                .filter_map(|d| serde_json::to_value(d).ok())
                .collect()
        }),
    );

    // ── Proof (i): catalog lists jellyfin ────────────────────────────────────
    let listed = call("plugin.list", json!({}), &ctx)
        .await
        .expect("plugin.list");
    let plugins = listed["plugins"].as_array().expect("plugins array");
    let jellyfin = plugins
        .iter()
        .find(|p| p["name"] == "jellyfin")
        .expect("jellyfin must be in the catalog");
    assert_eq!(jellyfin["catalog"]["targetSoftware"], "jellyfin");
    assert_eq!(jellyfin["catalog"]["status"], "available");
    // Not installed yet.
    assert_eq!(jellyfin["status"], "notInstalled");

    // ── Proof (iii): a bogus file is REFUSED and NOT installed ────────────────
    // (Run before the real install so a refused load can't poison the registry.)
    let bogus = tmp.path().join("libbogus.dylib");
    std::fs::write(&bogus, b"not a real cdylib, just bytes").unwrap();
    let refused = call(
        "plugin.install",
        json!({ "file": bogus.to_str().unwrap() }),
        &ctx,
    )
    .await;
    let err = refused.expect_err("a non-cdylib must be refused");
    let msg = format!("{err:#}");
    eprintln!("refusal: {msg}");
    assert!(
        msg.contains("compatibility gate failed") || msg.contains("ABI/layout"),
        "refusal must cite the compat gate: {msg}"
    );
    // Nothing landed in the install dir under the canonical name.
    let install_dir = system::plugin_manager::install_dir().unwrap();
    assert!(
        !install_dir.join("libjellyfin.dylib").exists(),
        "refused install must not have created a jellyfin artifact"
    );

    // ── Proof (ii): sideload passes the gate, lands, and tools are invocable ──
    let installed = call("plugin.install", json!({ "file": cdylib }), &ctx)
        .await
        .expect("sideload install must pass the gate");
    eprintln!("install result: {installed}");
    assert_eq!(installed["software"], "jellyfin");
    assert_eq!(installed["loadedLive"], true);
    let landed = installed["installedPath"].as_str().unwrap();
    assert!(
        std::path::Path::new(landed).is_file(),
        "cdylib must be copied into the install dir: {landed}"
    );
    assert!(landed.ends_with("libjellyfin.dylib"));

    // jellyfin.* tools are now invocable through the same dispatch entrypoint.
    // The unregistered-endpoint body returns an error — proof the FFI invoke
    // path executed inside the loaded library and marshalled the result back.
    let invoke = call(
        "jellyfin.transcode_health",
        json!({ "endpoint": "nope" }),
        &ctx,
    )
    .await;
    let invoke_err = invoke.expect_err("unregistered endpoint surfaces as error");
    let invoke_msg = format!("{invoke_err:#}");
    eprintln!("invoke round-trip: {invoke_msg}");
    assert!(
        invoke_msg.contains("jellyfin.transcode_health"),
        "FFI error names the tool: {invoke_msg}"
    );

    // plugin.list now reports jellyfin as loaded with its version + tools.
    let listed2 = call("plugin.list", json!({}), &ctx)
        .await
        .expect("plugin.list after install");
    let jf2 = listed2["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "jellyfin")
        .unwrap();
    assert_eq!(jf2["status"], "loaded");
    assert_eq!(jf2["installedVersion"], "0.1.0");
    assert!(
        jf2["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "jellyfin.transcode_health")
    );

    // ── Proof (iv): startup scan loads an already-installed plugin ────────────
    // Unload from the live registry (leaving the file on disk), then re-scan —
    // exactly what a fresh daemon boot does against a populated install dir.
    let removed = plugin_loader::unload_plugin("jellyfin");
    assert_eq!(removed, 1, "jellyfin should have been loaded");
    assert!(!plugin_loader::is_loaded("jellyfin"));
    let (loaded, failed) = system::plugin_manager::scan_and_load();
    eprintln!("startup scan: loaded={loaded:?} failed={failed:?}");
    assert!(
        loaded.contains(&"jellyfin".to_string()),
        "startup scan must reload the installed plugin"
    );
    assert!(plugin_loader::is_loaded("jellyfin"));
}
