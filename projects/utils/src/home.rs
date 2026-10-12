//! Orca state-dir resolution — the single resolver every crate shares
//! (`contract::config::paths` delegates here), living in the leaf crate so a
//! thin plugin reaches it without linking `contract`.
//!
//! `$ORCA_HOME` if set, else a per-process sandbox under `cargo test`, else
//! `$HOME/.orca`. A test binary never resolves a real home because opening the
//! state dir migrates the database.

use std::path::{Path, PathBuf};

const ENV_ORCA_HOME: &str = "ORCA_HOME";
const APP_STATE_DIR: &str = ".orca";

/// Resolve orca's state dir. `None` only when no home can be resolved.
pub fn orca_home() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(ENV_ORCA_HOME) {
        let explicit = PathBuf::from(explicit);
        // An ambient `$ORCA_HOME` from the developer's shell points at the LIVE
        // state dir and is checked before `$HOME`, so it would defeat every
        // harness that isolates by setting `$HOME` (#583).
        if diverts_to_sandbox(&explicit) {
            return Some(test_sandbox_home());
        }
        return Some(explicit);
    }
    // Only divert when there is a real home to protect: a test that clears both
    // vars is asserting the no-home path and still gets `None`.
    if is_cargo_test_binary() && home_state_dir().is_some() {
        return Some(test_sandbox_home());
    }
    home_state_dir()
}

/// True when an explicitly-requested path must be redirected to the sandbox:
/// this is a test binary and the path is not under the system temp dir (where
/// `tempfile` and the sandbox live).
#[doc(hidden)]
pub fn diverts_to_sandbox(requested: &Path) -> bool {
    is_cargo_test_binary()
        && !requested.starts_with(std::env::temp_dir())
        && home_state_dir().is_some()
}

/// `$HOME/.orca` — what a real orca process resolves to with no `$ORCA_HOME`.
#[doc(hidden)]
pub fn home_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(APP_STATE_DIR))
}

/// Cargo sets `CARGO_MANIFEST_DIR` for the processes it runs, and test binaries
/// live in `target/<profile>/deps/` while `cargo run` executes the profile root.
#[doc(hidden)]
pub fn is_cargo_test_binary() -> bool {
    if std::env::var_os("CARGO_MANIFEST_DIR").is_none() {
        return false;
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .and_then(|dir| dir.file_name())
                .map(|name| name == std::ffi::OsStr::new("deps"))
        })
        .unwrap_or(false)
}

/// One state dir per test process, shared by every resolved path. Keeps the
/// `.orca` leaf so shape assertions (`…/.orca/pki`) hold.
#[doc(hidden)]
pub fn test_sandbox_home() -> PathBuf {
    static SANDBOX: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SANDBOX
        .get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("orca-test-home-{}", std::process::id()))
                .join(APP_STATE_DIR);
            // A failure here surfaces at the first real read, which reports the path.
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("could not create test state dir {}: {e}", dir.display());
            }
            dir
        })
        .clone()
}
