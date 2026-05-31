//! Emit `ORCA_VERSION` and `ORCA_BUILD_TARGET` as compile-time env vars so
//! `system::system::system_detail` can stamp them into `SystemStatusReport`
//! without going through a service trait (slice A4 — no indirection).
//!
//! Logic mirrors `projects/server/build.rs::resolve_version` so a build of
//! either crate reports the same version string.

use std::env;
use std::process::Command;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    let version = resolve_version();
    println!("cargo:rustc-env=ORCA_VERSION={version}");
    println!("cargo:rerun-if-changed={manifest}/../../.git/HEAD");
    println!("cargo:rerun-if-changed={manifest}/../../.git/index");

    let target = env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());
    println!("cargo:rustc-env=ORCA_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORCA_RELEASE_VERSION");
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
