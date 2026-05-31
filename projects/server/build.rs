use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    // Resolve a real runtime version from git so `orca` reports what it is.
    //   on a clean tag             → "0.0.3-rc.3"
    //   N commits past last tag    → "0.0.3-rc.3-dev+5.g66d2ea6"
    //   working tree dirty         → "...-dev+5.g66d2ea6.dirty"
    //   no git / shallow checkout  → "<CARGO_PKG_VERSION>+unknown"
    let version = resolve_version();
    println!("cargo:rustc-env=ORCA_VERSION={version}");
    // Rerun when HEAD or the working tree changes so the version stays fresh.
    println!("cargo:rerun-if-changed={manifest}/../../.git/HEAD");
    println!("cargo:rerun-if-changed={manifest}/../../.git/index");

    // Agent + slash-command .md embedding moved to projects/agents/build.rs.

    // Expose the build target triple to the binary (was previously expected
    // to be supplied externally; emitting it from build.rs keeps `cargo
    // check` working without env setup).
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());
    println!("cargo:rustc-env=ORCA_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORCA_RELEASE_VERSION");

    // Only ensure frontend/dist exists when the `ui` feature is on — that's
    // the only build configuration where RustEmbed reads from it. Headless
    // builds skip this so they don't touch the frontend tree at all.
    if env::var_os("CARGO_FEATURE_UI").is_some() {
        let dist = Path::new(&manifest).join("../frontend/dist");
        fs::create_dir_all(&dist).expect("failed to create frontend/dist stub");
    }
}

fn resolve_version() -> String {
    if let Ok(v) = env::var("ORCA_RELEASE_VERSION")
        && !v.trim().is_empty()
    {
        return v.trim().trim_start_matches('v').to_string();
    }
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let exact_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if let Some(tag) = exact_tag
        && !dirty
    {
        return tag.strip_prefix('v').unwrap_or(&tag).to_string();
    }

    let sha = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let mut s = format!("{cargo_version}-dev+g{sha}");
    if dirty {
        s.push_str(".dirty");
    }
    s
}
