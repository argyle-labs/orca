use std::env;

fn main() {
    let target = resolve_build_target();
    println!("cargo:rustc-env=ORCA_BUILD_TARGET={target}");

    println!("cargo:rerun-if-changed=build.rs");
    // Re-run if the target changes so a rebuilt-for-a-different-triple binary
    // never keeps a stale ORCA_BUILD_TARGET baked from a prior build.
    println!("cargo:rerun-if-env-changed=TARGET");
}

/// The Rust target triple this binary is compiled for, stamped into the daemon
/// so `system.detail` / the mesh can report it and the self-update path can
/// resolve the matching release asset.
///
/// `TARGET` is set by cargo for every build script — but a build binary that
/// silently baked `unknown-target` (the old fallback) is worse than useless:
/// it blocks release-asset and plugin-asset resolution by triple, and the
/// failure is invisible until a self-update mysteriously can't find its asset
/// (exactly how mint shipped an un-updatable `unknown-target` binary). So when
/// `TARGET` is somehow absent we reconstruct the triple from the
/// `CARGO_CFG_TARGET_*` vars cargo also sets for build scripts, and shout via
/// `cargo:warning` rather than baking a poison value quietly.
fn resolve_build_target() -> String {
    if let Ok(t) = env::var("TARGET")
        && !t.trim().is_empty()
    {
        return t;
    }
    match reconstruct_target_triple() {
        Some(t) => {
            println!(
                "cargo:warning=TARGET env var missing; reconstructed build target triple as {t} \
                 from CARGO_CFG_TARGET_* — verify the build toolchain"
            );
            t
        }
        None => {
            println!(
                "cargo:warning=cannot determine build target triple (TARGET and \
                 CARGO_CFG_TARGET_* all unset); baking unknown-target — self-update and \
                 plugin-asset resolution by triple will NOT work for this binary"
            );
            "unknown-target".to_string()
        }
    }
}

/// Reconstruct `<arch>-<vendor>-<os>[-<env>]` from the `CARGO_CFG_TARGET_*`
/// vars cargo exports for build scripts. Handles the darwin quirk where
/// `CARGO_CFG_TARGET_OS` is `macos` but the triple's OS field is `darwin`.
fn reconstruct_target_triple() -> Option<String> {
    let arch = env::var("CARGO_CFG_TARGET_ARCH")
        .ok()
        .filter(|s| !s.is_empty())?;
    let vendor = env::var("CARGO_CFG_TARGET_VENDOR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let os = env::var("CARGO_CFG_TARGET_OS")
        .ok()
        .filter(|s| !s.is_empty())?;
    // The triple spells Apple's OS `darwin`, but the cfg reports `macos`.
    let os = if os == "macos" {
        "darwin".to_string()
    } else {
        os
    };
    let env_abi = env::var("CARGO_CFG_TARGET_ENV")
        .ok()
        .filter(|s| !s.is_empty());
    Some(match env_abi {
        Some(e) => format!("{arch}-{vendor}-{os}-{e}"),
        None => format!("{arch}-{vendor}-{os}"),
    })
}
