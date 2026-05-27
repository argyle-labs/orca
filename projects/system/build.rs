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
}

fn resolve_version() -> String {
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());

    let described = match Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return format!("{cargo_version}+unknown"),
    };

    let described = described
        .strip_prefix('v')
        .unwrap_or(&described)
        .to_string();

    let exact_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let dirty = described.ends_with("-dirty");

    if exact_tag && !dirty {
        return described;
    }

    let stripped = described.trim_end_matches("-dirty");
    let parts: Vec<&str> = stripped.rsplitn(3, '-').collect();
    if parts.len() == 3 && parts[0].starts_with('g') {
        let sha = parts[0];
        let n = parts[1];
        let tag = parts[2];
        let mut s = format!("{tag}-dev+{n}.{sha}");
        if dirty {
            s.push_str(".dirty");
        }
        s
    } else {
        let mut s = format!("{cargo_version}-dev+g{stripped}");
        if dirty {
            s.push_str(".dirty");
        }
        s
    }
}
