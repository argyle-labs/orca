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
    assert_eq!(report.steps.len(), 5);
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
    assert_eq!(report.steps.len(), 5);
}

/// Build the TypeScript reference plugin (projects/sdk-ts) and run it
/// through the same conformance checker. Skipped if `node` or `npm` are
/// missing — the Rust port remains the gating reference.
#[tokio::test(flavor = "current_thread")]
async fn typescript_reference_plugin_is_conformant() {
    install_ring();

    if Command::new("node").arg("--version").output().is_err()
        || Command::new("npm").arg("--version").output().is_err()
    {
        eprintln!("skipping ts conformance test: node/npm not on PATH");
        return;
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has parent")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let ts_module = workspace_root.join("projects/sdk-ts");
    assert!(
        ts_module.join("package.json").exists(),
        "missing TS SDK at {}",
        ts_module.display()
    );

    if !ts_module.join("node_modules").exists() {
        let status = Command::new("npm")
            .arg("install")
            .arg("--no-audit")
            .arg("--no-fund")
            .current_dir(&ts_module)
            .status()
            .expect("spawn npm install");
        assert!(status.success(), "npm install failed: {status}");
    }
    let status = Command::new("npx")
        .args(["tsc", "-p", "tsconfig.json"])
        .current_dir(&ts_module)
        .status()
        .expect("spawn tsc");
    assert!(status.success(), "tsc failed: {status}");

    // Wrapper script so run_subprocess can spawn a single executable.
    let outdir = tempfile::tempdir().expect("tempdir for ts launcher");
    let launcher = outdir.path().join("orca-conformance-plugin-ts");
    let entry = ts_module.join("dist/bin/conformance-plugin.js");
    std::fs::write(
        &launcher,
        format!(
            "#!/usr/bin/env bash\nexec node {} \"$@\"\n",
            entry.display()
        ),
    )
    .expect("write launcher");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
            .expect("chmod launcher");
    }

    let cfg = SubprocessConfig {
        plugin_binary: launcher,
        workdir: None,
        timeout: Duration::from_secs(20),
    };
    let report = run_subprocess(cfg).await.expect("run_subprocess");
    assert!(
        report.passed,
        "TypeScript reference plugin not conformant. Steps: {:#?}",
        report.steps
    );
    assert_eq!(report.steps.len(), 5);
}

/// Build the Kotlin reference plugin (projects/sdk-kotlin) and run it
/// through the same conformance checker. Skipped if `gradle` or `java` are
/// missing — the Rust port remains the gating reference.
#[tokio::test(flavor = "current_thread")]
async fn kotlin_reference_plugin_is_conformant() {
    install_ring();

    if Command::new("gradle").arg("--version").output().is_err()
        || Command::new("java").arg("--version").output().is_err()
    {
        eprintln!("skipping kotlin conformance test: gradle/java not on PATH");
        return;
    }

    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has parent")
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let kt_module = workspace_root.join("projects/sdk-kotlin");
    assert!(
        kt_module.join("build.gradle.kts").exists(),
        "missing Kotlin SDK at {}",
        kt_module.display()
    );

    let status = Command::new("gradle")
        .args(["--no-daemon", "-q", "conformanceJar"])
        .current_dir(&kt_module)
        .status()
        .expect("spawn gradle");
    assert!(status.success(), "gradle conformanceJar failed: {status}");

    // Locate the built fat-jar (conformance-all classifier).
    let libs = kt_module.join("build/libs");
    let jar = std::fs::read_dir(&libs)
        .expect("read build/libs")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with("-conformance-all.jar"))
        })
        .expect("conformance-all.jar present");

    let outdir = tempfile::tempdir().expect("tempdir for kotlin launcher");
    let launcher = outdir.path().join("orca-conformance-plugin-kt");
    std::fs::write(
        &launcher,
        format!(
            "#!/usr/bin/env bash\nexec java -jar {} \"$@\"\n",
            jar.display()
        ),
    )
    .expect("write launcher");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
            .expect("chmod launcher");
    }

    let cfg = SubprocessConfig {
        plugin_binary: launcher,
        workdir: None,
        timeout: Duration::from_secs(30),
    };
    let report = run_subprocess(cfg).await.expect("run_subprocess");
    assert!(
        report.passed,
        "Kotlin reference plugin not conformant. Steps: {:#?}",
        report.steps
    );
    assert_eq!(report.steps.len(), 5);
}
