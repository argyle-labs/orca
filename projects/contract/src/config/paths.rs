//! Canonical orca state-dir + path resolution.
//!
//! One source of truth for "where does this orca instance keep its state".
//! Every crate that needs the state dir, DB path, PKI dir, memory root, etc.
//! MUST resolve through here so a single `$ORCA_HOME` (or `$ORCA_DB_PATH`)
//! moves the whole instance, and two instances under different `$ORCA_HOME`
//! are fully independent.
//!
//! Historically this was split-brained: `files::ops::orca_home()` honored
//! `$ORCA_HOME` while `Config::load()` / PKI / db / state all hard-coded
//! `dirs::home_dir().join(".orca")` and silently ignored it — so `$ORCA_HOME`
//! isolated the loopback token but not the DB. This module unifies them;
//! `files::ops::orca_home()` now delegates here.
//!
//! Precedence:
//!   - state dir: `$ORCA_HOME` if set, else a per-process sandbox under
//!     `cargo test`, else `$HOME/.orca`.
//!   - db path:   `$ORCA_DB_PATH` if set (absolute override), else
//!     `<state_dir>/orca.db`.
//!
//! The test sandbox exists because opening the state dir MIGRATES the database.
//! On 2026-09-26 a suite run with no `$ORCA_HOME` applied an in-development
//! migration to a live `~/.orca/orca.db`; that host's daemon predated the
//! migration and then failed every write to an existing config row. A test
//! binary therefore never resolves a real home.
//!
//! A test that SPAWNS the orca binary must still set `$ORCA_HOME` on the child —
//! the child is a real binary, so the sandbox does not cover it.

use anyhow::{Context, Result};
use std::path::PathBuf;

use super::consts;

/// Environment variable naming the orca state directory root. When set it
/// overrides `$HOME/.orca` for EVERY path this module resolves.
pub const ENV_ORCA_HOME: &str = "ORCA_HOME";

/// Environment variable naming an explicit DB file path, overriding the
/// `<state_dir>/orca.db` default. Used for test isolation and unusual layouts.
pub const ENV_ORCA_DB_PATH: &str = "ORCA_DB_PATH";

/// Resolve orca's state dir: `$ORCA_HOME` if set, else a per-process sandbox
/// under `cargo test`, else `$HOME/.orca`.
/// Returns `None` only when no home can be resolved (sealed CI, sandboxes).
/// Prefer [`state_dir`] when you want an error instead of `None`.
pub fn orca_home() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(ENV_ORCA_HOME) {
        return Some(PathBuf::from(explicit));
    }
    // Only divert when there is a real home to protect: a test that clears both
    // vars is asserting the no-home path and still gets `None`.
    if is_cargo_test_binary() && home_state_dir().is_some() {
        return Some(test_sandbox_home());
    }
    home_state_dir()
}

/// `$HOME/.orca` — what a real orca process resolves to with no `$ORCA_HOME`.
///
/// Public so a test can assert this resolution directly: under `cargo test`
/// [`orca_home`] deliberately answers with a sandbox instead.
#[doc(hidden)]
pub fn home_state_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(consts::APP_STATE_DIR))
}

/// True when this process is a `cargo test` binary.
///
/// Cargo sets `CARGO_MANIFEST_DIR` for the processes it runs, and test binaries
/// live in `target/<profile>/deps/` while `cargo run` executes the profile root —
/// so both signals together separate a test run from a real one.
fn is_cargo_test_binary() -> bool {
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

/// Per-process state dir for a test run, so a suite never opens — or migrates —
/// the developer's live `~/.orca/orca.db`. One directory per process, shared by
/// every path this module resolves, so a test that writes the DB and one that
/// reads it still agree.
///
/// It keeps the `.orca` leaf a real state dir has, so assertions about the
/// resolved shape (`…/.orca/pki`) hold here too.
fn test_sandbox_home() -> PathBuf {
    static SANDBOX: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    SANDBOX
        .get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("orca-test-home-{}", std::process::id()))
                .join(consts::APP_STATE_DIR);
            // A failure here surfaces at the first real read, which reports the path.
            if let Err(e) = std::fs::create_dir_all(&dir) {
                eprintln!("could not create test state dir {}: {e}", dir.display());
            }
            dir
        })
        .clone()
}

/// Like [`orca_home`] but errors (with context) when no home can be resolved.
pub fn state_dir() -> Result<PathBuf> {
    orca_home().context("no $ORCA_HOME and no $HOME to resolve orca state dir")
}

/// The orca DB file path: `$ORCA_DB_PATH` if set, else `<state_dir>/orca.db`.
pub fn db_path() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(ENV_ORCA_DB_PATH) {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    Ok(state_dir()?.join(consts::APP_DB_FILE))
}

/// PKI material dir: `<state_dir>/pki`.
pub fn pki_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join(consts::APP_PKI_DIR))
}

/// Memory root: `<state_dir>/memory`.
pub fn memory_root() -> Result<PathBuf> {
    Ok(state_dir()?.join("memory"))
}

/// Per-profile content root: `<state_dir>/profiles`.
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join(consts::APP_PROFILES_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests mutate process env; keep them in one test fn so they don't
    // race each other under the parallel test runner.
    #[test]
    fn resolution_precedence() {
        let base = std::env::temp_dir().join(format!("orca-paths-{}", std::process::id()));
        let orca = base.join("state");
        let home = base.join("home");

        // $ORCA_HOME wins over $HOME/.orca.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var(ENV_ORCA_HOME, &orca);
            std::env::remove_var(ENV_ORCA_DB_PATH);
        }
        assert_eq!(orca_home().unwrap(), orca);
        assert_eq!(db_path().unwrap(), orca.join(consts::APP_DB_FILE));
        assert_eq!(pki_dir().unwrap(), orca.join(consts::APP_PKI_DIR));

        // $ORCA_DB_PATH overrides just the DB file.
        let dbp = orca.join("custom").join("x.db");
        unsafe { std::env::set_var(ENV_ORCA_DB_PATH, &dbp) };
        assert_eq!(db_path().unwrap(), dbp);

        // A real process with no $ORCA_HOME resolves $HOME/.orca.
        unsafe {
            std::env::remove_var(ENV_ORCA_HOME);
            std::env::remove_var(ENV_ORCA_DB_PATH);
        }
        assert_eq!(home_state_dir().unwrap(), home.join(consts::APP_STATE_DIR));

        // This test binary, with no $ORCA_HOME, gets a sandbox instead — a suite
        // must never open the developer's live state dir, let alone migrate it.
        let resolved = orca_home().unwrap();
        assert!(is_cargo_test_binary(), "this is a cargo test binary");
        assert_ne!(resolved, home.join(consts::APP_STATE_DIR));
        assert!(
            resolved.starts_with(std::env::temp_dir()),
            "sandbox lives under the temp dir: {resolved:?}"
        );
        assert!(resolved.is_dir(), "sandbox is created eagerly");
    }

    #[test]
    fn the_sandbox_is_stable_within_a_process() {
        // Every path resolves against one directory, so a test that writes the DB
        // and one that reads it agree.
        assert_eq!(test_sandbox_home(), test_sandbox_home());
    }

    #[test]
    fn a_test_binary_never_resolves_the_real_home() {
        // The guard that failed on 2026-09-26: with $ORCA_HOME unset the suite
        // opened ~/.orca/orca.db and applied a pending migration to it.
        unsafe { std::env::remove_var(ENV_ORCA_HOME) };
        let real = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(consts::APP_STATE_DIR))
            .expect("HOME set");
        assert_ne!(orca_home().unwrap(), real);
    }
}
