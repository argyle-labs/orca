//! End-to-end conformance: build the Rust reference plugin (this crate's
//! binary) and run it as a subprocess against the SDK's conformance host.
//! A pass here proves the wire contract works through a real exec boundary,
//! exactly the same way Go/Kotlin/TS ports will be exercised.
//!
//! A second test compiles the Go reference plugin in
//! `projects/sdk-go/cmd/conformance-plugin` and runs it through the same
//! checker. Both binaries must produce identical observations.

use orca_sdk::conformance::{SubprocessConfig, run_subprocess};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[tokio::test(flavor = "current_thread")]
async fn rust_reference_plugin_is_conformant() {
    install_ring();

    // Cargo sets CARGO_BIN_EXE_<name> for [[bin]] entries when building tests.
    let plugin_bin = PathBuf::from(env!("CARGO_BIN_EXE_orca-conformance-plugin"));
    assert!(
        plugin_bin.exists(),
        "expected built plugin at {}",
        plugin_bin.display()
    );

    let cfg = SubprocessConfig {
        plugin_binary: plugin_bin,
        workdir: None,
        timeout: Duration::from_secs(15),
    };
    let report = run_subprocess(cfg).await.expect("run_subprocess");

    assert!(
        report.passed,
        "Rust reference plugin not conformant. Steps: {:#?}",
        report.steps
    );
    assert_eq!(report.steps.len(), 3);
}

/// Build the Go reference plugin (projects/sdk-go/cmd/conformance-plugin)
/// and run it through the same conformance checker. If the `go` toolchain
/// is not on PATH, the test is skipped — the Rust port remains the gating
/// reference; cross-language conformance is additive.
#[tokio::test(flavor = "current_thread")]
async fn go_reference_plugin_is_conformant() {
    install_ring();

    if Command::new("go").arg("version").output().is_err() {
        eprintln!("skipping go conformance test: `go` not on PATH");
        return;
    }

    // projects/conformance-plugin/tests/conformance.rs → workspace root
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has parent")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let go_module = workspace_root.join("projects/sdk-go");
    let go_main = go_module.join("cmd/conformance-plugin");
    assert!(
        go_main.join("main.go").exists(),
        "missing Go conformance plugin at {}",
        go_main.display()
    );

    let outdir = tempfile::tempdir().expect("tempdir for go binary");
    let bin_path = outdir.path().join(if cfg!(windows) {
        "orca-conformance-plugin-go.exe"
    } else {
        "orca-conformance-plugin-go"
    });

    let status = Command::new("go")
        .args(["build", "-o"])
        .arg(&bin_path)
        .arg("./cmd/conformance-plugin")
        .current_dir(&go_module)
        .status()
        .expect("spawn go build");
    assert!(status.success(), "go build failed: {status}");

    let cfg = SubprocessConfig {
        plugin_binary: bin_path,
        workdir: None,
        timeout: Duration::from_secs(15),
    };
    let report = run_subprocess(cfg).await.expect("run_subprocess");

    assert!(
        report.passed,
        "Go reference plugin not conformant. Steps: {:#?}",
        report.steps
    );
    assert_eq!(report.steps.len(), 3);
}
