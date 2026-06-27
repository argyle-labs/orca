//! End-to-end proof of the cdylib/dlopen plugin seam against the real ntfy
//! plugin artifact — the **combined** plugin shape: a `#[orca_tool]` tool
//! surface (`ntfy.*`) AND a `notifications`-domain backend (one per enabled
//! endpoint row).
//!
//! Where `load_jellyfin` proves a pure tool-surface plugin and `load_nfs`
//! proves a pure storage-backend plugin, ntfy proves both seams in one library
//! plus the **notifications** domain proxy: a successful load both registers
//! the `ntfy.*` tools and — for every enabled endpoint row — registers a
//! `NotifyProxy` into the process-global notifications dispatcher. Each
//! `emit` against that backend marshals the `Event` to JSON and calls back into
//! the loaded library's `invoke()` under `notify.__backend.<endpoint>.emit`,
//! which rebuilds the endpoint's `NtfyBackend` from the db and sends.
//!
//! Gated on `NTFY_CDYLIB` (absolute path to `libntfy.dylib`, produced by
//! `cargo build --lib` in the ntfy repo). Unset → early-return so CI without the
//! artifact stays green. Uses an isolated `ORCA_DB_PATH` temp db so the seeded
//! endpoint is shared across the FFI boundary (env is process-global).
//!
//! ```sh
//! NTFY_CDYLIB=/abs/path/to/libntfy.dylib \
//!   cargo test -p plugin-loader --test load_ntfy -- --nocapture
//! ```

use std::path::Path;

use plugin_loader::{invoke_plugin, load_plugin, loaded_tool_defs, unload_plugin};
use plugin_toolkit::notify::{self, Event, EventClass, Severity};
use serde_json::json;

#[test]
fn ntfy_cdylib_loads_tools_and_registers_notification_backend() {
    let Ok(cdylib) = std::env::var("NTFY_CDYLIB") else {
        eprintln!("NTFY_CDYLIB unset — skipping combined cdylib e2e proof");
        return;
    };
    let path = Path::new(&cdylib);

    // Isolated temp db, shared with the loaded library via the process-global
    // `ORCA_DB_PATH` env (the cdylib's `open_default()` reads the same var).
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("orca-test.db");
    // SAFETY: single-threaded test; set before any db access.
    unsafe { std::env::set_var("ORCA_DB_PATH", &db_path) };

    // ── Seam 1: load succeeds, reports the header + tool surface ───────────────
    let report = load_plugin(path, "0.0.8-rc.8").expect("load must succeed for supported orca");
    eprintln!("load report: {report:?}");
    assert_eq!(report.software, "ntfy");
    assert!(
        report.tools.contains(&"ntfy.send".to_string())
            && report.tools.contains(&"ntfy.create".to_string()),
        "report.tools must include the ntfy.* surface: {:?}",
        report.tools
    );

    // ── Seam 2: the parsed manifest exposes the tool surface ───────────────────
    let defs = loaded_tool_defs();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    eprintln!("manifest tool names: {names:?}");
    assert!(names.contains(&"ntfy.send"));

    // No endpoint seeded yet → the db has no ntfy table, so `backends()`
    // observed an empty set and registered no notification backend.
    assert!(
        !notify::registered_backend_names().contains(&"testep".to_string()),
        "no endpoint seeded yet — no notification backend should exist"
    );

    // ── Seed an endpoint through the plugin's own `ntfy.create` tool ───────────
    // Runs inside the loaded library; creates the table + row in the shared db.
    // Points at an unreachable URL so the later emit surfaces a transport error
    // — the error crossing back is itself the proof the call reached the sender.
    let created = invoke_plugin(
        "ntfy.create",
        &json!({
            "name": "testep",
            "base_url": "http://127.0.0.1:9",
            "topic": "t",
            "enabled": true
        }),
    )
    .expect("invoke_plugin must find ntfy.create");
    created.expect("ntfy.create must succeed against the temp db");

    // ── Reload so `backends()` re-reads the db and registers the endpoint ──────
    let removed = unload_plugin("ntfy");
    eprintln!("unloaded {removed} registration(s) before reseeded reload");
    let report = load_plugin(path, "0.0.8-rc.8").expect("reload must succeed");
    eprintln!("reload backend names: {:?}", report.tools);

    // ── Seam 3: the seeded endpoint crossed into the notifications dispatcher ──
    let backend_names = notify::registered_backend_names();
    eprintln!("registered notification backends: {backend_names:?}");
    assert!(
        backend_names.contains(&"testep".to_string()),
        "the enabled endpoint must register a notification backend: {backend_names:?}"
    );

    // ── Seam 4: emit round-trips through the NotifyProxy → loader thunk → cdylib
    // `invoke` → `NtfyBackend::emit` → HTTP. The endpoint URL is unreachable, so
    // the per-backend outcome carries a transport error — proof the emit crossed
    // the FFI boundary and executed the sender inside the loaded library.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    let event = Event::new(EventClass::Alert, Severity::Warn, "e2e", "load_ntfy");
    let outcomes = rt.block_on(notify::emit(&event));
    eprintln!("emit outcomes: {outcomes:?}");
    let testep = outcomes
        .iter()
        .find(|o| o.backend == "testep")
        .expect("emit must dispatch to the seeded backend");
    assert!(
        testep.result.is_err(),
        "unreachable endpoint must surface a transport error from inside the library"
    );

    // ── Seam 5: unload reverses both tool + backend registration ───────────────
    unload_plugin("ntfy");
    assert!(
        !notify::registered_backend_names().contains(&"testep".to_string()),
        "unload must deregister the ntfy notification backend"
    );
}
