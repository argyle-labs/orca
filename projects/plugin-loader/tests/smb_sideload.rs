//! Live load verification for the standalone `smb` plugin cdylib.
//!
//! Ignored by default — it dlopens a real build artifact. Build the standalone
//! repo first, then point this at the artifact:
//!
//!   (cd ~/code/argyle-labs/smb && cargo build --release)
//!   SMB_DYLIB=~/code/argyle-labs/smb/target/release/libsmb.dylib \
//!     cargo test -p plugin-loader --test smb_sideload -- --ignored --nocapture
//!
//! Exercises the full real path the daemon's sideload uses: dlopen → ABI gate →
//! `manifest()` (empty, smb has no tools) → `backends()` → the loader's
//! `deploy_target`/`storage` domain dispatch → the process-global storage
//! registry. Asserts the smb storage backend is actually registered and usable.

use std::path::PathBuf;

#[test]
#[ignore]
fn smb_cdylib_loads_and_registers_storage_backend() {
    let path = std::env::var("SMB_DYLIB").expect("set SMB_DYLIB to the built libsmb.dylib path");
    let path = PathBuf::from(shellexpand_tilde(&path));
    assert!(path.exists(), "dylib not found: {}", path.display());

    // smb advertises orca_compat `>=0.0.8, <0.1.0`; pass a release version in
    // that range so the gate's prerelease ordering doesn't mask the ABI check.
    let report = plugin_loader::load_plugin(&path, "0.0.8")
        .expect("smb cdylib should pass the abi_stable + semver gate and load");
    println!("loaded: {report:?}");

    // The schema-declaration ABI seam crosses a real dlopen: smb declares no
    // tables, so the parsed declaration is empty (this proves the `schemas()`
    // field is wired through the FFI, defaulting cleanly for a stateless plugin).
    assert!(
        report.declared_schema.tables.is_empty(),
        "smb declares no SQL tables"
    );

    // smb registers exactly one storage backend named "smb" via backends().
    let providers = plugin_toolkit::storage::providers();
    println!("storage providers after load: {providers:?}");
    let smb = providers
        .iter()
        .find(|p| p.name == "smb")
        .expect("smb storage backend registered in the global storage registry");
    assert!(
        matches!(smb.kind, plugin_toolkit::storage::StorageKind::NetworkShare),
        "smb is a network-share backend"
    );
    assert!(
        smb.capabilities
            .contains(&plugin_toolkit::storage::Capability::List),
        "smb advertises the `list` capability"
    );

    // Clean up so a re-run starts from a known state.
    plugin_loader::unload_plugin("smb");
}

fn shellexpand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    p.to_string()
}
