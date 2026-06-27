//! End-to-end proof of the cdylib/dlopen plugin seam against the real nfs
//! plugin artifact — the **backend-only** plugin shape.
//!
//! Where `load_jellyfin` proves a tool-surface plugin (manifest + `invoke`),
//! this proves the storage-backend seam: nfs carries zero `#[orca_tool]`s, so
//! its whole surface crosses through `backends()`. A successful load must
//! register an `nfs` storage backend into the process-global storage registry,
//! and every call against that registered backend is a host-side `StorageProxy`
//! that marshals its args to JSON and calls back into the loaded library's
//! `invoke()` under the `storage.__backend.nfs.*` namespace.
//!
//! Gated on `NFS_CDYLIB` (absolute path to `libnfs.dylib`, produced by
//! `cargo build -p nfs --lib`). Unset → early-return so CI without the artifact
//! stays green.
//!
//! ```sh
//! NFS_CDYLIB=/abs/path/to/libnfs.dylib \
//!   cargo test -p plugin-loader --test load_nfs -- --nocapture
//! ```

use std::path::Path;
use std::time::Duration;

use plugin_loader::{load_plugin, unload_plugin};
use plugin_toolkit::storage;

#[test]
fn nfs_cdylib_loads_and_registers_storage_backend() {
    let Ok(cdylib) = std::env::var("NFS_CDYLIB") else {
        eprintln!("NFS_CDYLIB unset — skipping backend cdylib e2e proof");
        return;
    };
    let path = Path::new(&cdylib);

    // ── Seam 1: the load registers a backend, carrying zero tools ─────────────
    let report = load_plugin(path, "0.0.8-rc.8").expect("load must succeed for supported orca");
    eprintln!("load report: {report:?}");
    assert_eq!(report.software, "nfs");
    assert!(
        report.tools.is_empty(),
        "nfs is backend-only — it must contribute no tools, got: {:?}",
        report.tools
    );

    // ── Seam 2: the backend crossed into the process-global storage registry ──
    let names: Vec<String> = storage::backends()
        .iter()
        .map(|b| b.name().to_string())
        .collect();
    eprintln!("registered storage backends: {names:?}");
    let backend = storage::backend("nfs").expect("nfs backend must be registered after load");

    // ── Seam 3: a call against the registered backend round-trips back into the
    // loaded library through the StorageProxy → loader thunk → cdylib `invoke`.
    // `list_shares` reads the real mount table *inside the loaded library* and
    // marshals its result back as JSON. The crossing is the proof: on Linux it
    // returns `Ok(Vec<Share>)`; on a host with no `/proc/mounts` (macOS) the
    // library's own mount-table read fails and that error marshals back across
    // the boundary — exactly the cross-FFI execution this test exists to prove
    // (mirroring `load_jellyfin`, which treats a returned error as crossing).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime");
    match rt.block_on(backend.list_shares()) {
        Ok(shares) => eprintln!("list_shares round-trip returned {} share(s)", shares.len()),
        Err(e) => {
            let msg = format!("{e}");
            eprintln!("list_shares round-trip surfaced library-side error: {msg}");
            assert!(
                msg.contains("mounts") || msg.contains("proc"),
                "an error must originate from the library's mount-table read, got: {msg}"
            );
        }
    }

    // `recover_stale` over an empty watch set exercises the args-bearing proxy
    // op (JSON-encoded `{watch, health_timeout_secs}`); whether it self-heals to
    // a no-op outcome or surfaces the same mount-table read error, the args-
    // bearing proxy path crossed the boundary.
    match rt.block_on(backend.recover_stale(&[], Duration::from_secs(5))) {
        Ok(outcome) => eprintln!("recover_stale round-trip outcome: {outcome:?}"),
        Err(e) => eprintln!("recover_stale round-trip surfaced library-side error: {e}"),
    }

    // ── Seam 4: unload reverses the registration ──────────────────────────────
    let removed = unload_plugin("nfs");
    eprintln!("unload removed {removed} registration(s)");
    assert!(
        storage::backend("nfs").is_none(),
        "unload must deregister the nfs storage backend"
    );
}
