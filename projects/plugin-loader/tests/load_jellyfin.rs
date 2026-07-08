//! End-to-end proof of the cdylib/dlopen plugin seam against the real jellyfin
//! plugin artifact.
//!
//! Gated on the `JELLYFIN_CDYLIB` env var (absolute path to `libjellyfin.dylib`,
//! produced by `cargo build --lib` in the jellyfin repo). When unset the test
//! early-returns so `cargo test` in CI without the artifact stays green.
//!
//! Build the artifact, then:
//! ```sh
//! JELLYFIN_CDYLIB=/abs/path/to/libjellyfin.dylib \
//!   cargo test -p plugin-loader --test load_jellyfin -- --nocapture
//! ```
//!
//! Proves four seams against the single process-global plugin registry:
//! 1. the orca-version compat gate refuses an out-of-range orca cleanly,
//! 2. a load under the supported orca version succeeds + reports the header,
//! 3. the parsed manifest exposes the plugin's tool surface, and
//! 4. an `invoke()` round-trips a tool body's result/error across the FFI seam.

use std::path::Path;

use plugin_loader::{invoke_plugin, load_plugin, loaded_tool_defs};
use serde_json::json;

#[test]
fn jellyfin_cdylib_loads_gates_lists_and_invokes() {
    let Ok(cdylib) = std::env::var("JELLYFIN_CDYLIB") else {
        eprintln!("JELLYFIN_CDYLIB unset — skipping cdylib e2e proof");
        return;
    };
    let path = Path::new(&cdylib);

    // ── Seam 1: orca-version compat gate refuses cleanly ──────────────────────
    // Run this FIRST: the registry is process-global and refuses duplicate tool
    // names, so a refused load (which never reaches registration) leaves the
    // registry clean for the successful load below. Orca 0.5.0 is outside the
    // plugin's declared `>=0.0.8, <0.2.0` range.
    let refused = load_plugin(path, "0.5.0");
    let err = refused.expect_err("load must refuse an out-of-range orca version");
    let msg = format!("{err:#}");
    eprintln!("compat-gate refusal: {msg}");
    assert!(
        msg.contains("requires orca") && msg.contains("0.5.0"),
        "refusal must name the orca-version requirement, got: {msg}"
    );

    // ── Seam 2: a supported-version load succeeds + reports the header ─────────
    let report = load_plugin(path, "0.0.8-rc.7").expect("load must succeed for supported orca");
    eprintln!("load report: {report:?}");
    assert_eq!(report.software, "jellyfin");
    assert_eq!(report.semver, "0.1.0");
    assert!(
        report.tools.contains(&"jellyfin.server_info".to_string()),
        "report.tools must include jellyfin.server_info: {:?}",
        report.tools
    );
    assert!(
        report
            .tools
            .contains(&"jellyfin.transcode_health".to_string()),
        "report.tools must include jellyfin.transcode_health: {:?}",
        report.tools
    );

    // ── Seam 3: the parsed manifest exposes the tool surface ──────────────────
    let defs = loaded_tool_defs();
    assert!(!defs.is_empty(), "loaded_tool_defs must be non-empty");
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    eprintln!("manifest tool names: {names:?}");
    assert!(names.contains(&"jellyfin.server_info"));
    assert!(names.contains(&"jellyfin.transcode_health"));

    // ── Seam 4: invoke() round-trips a tool body across the FFI boundary ──────
    // No endpoint "nope" is registered in the cdylib's own db, so the tool body
    // returns an error — which crossing back as a JSON-string error is itself
    // the proof that the FFI invoke path executed the tool inside the loaded
    // library and marshalled its result back.
    let invoked = invoke_plugin("jellyfin.transcode_health", &json!({ "endpoint": "nope" }));
    let result = invoked.expect("invoke_plugin must find the loaded tool");
    let invoke_err = result.expect_err("unregistered endpoint must surface as an error");
    let invoke_msg = format!("{invoke_err:#}");
    eprintln!("invoke round-trip error: {invoke_msg}");
    assert!(
        invoke_msg.contains("jellyfin.transcode_health"),
        "FFI error must name the failing tool: {invoke_msg}"
    );
}
