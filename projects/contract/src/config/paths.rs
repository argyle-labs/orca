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
//!   - state dir: `$ORCA_HOME` if set, else `$HOME/.orca`.
//!   - db path:   `$ORCA_DB_PATH` if set (absolute override), else
//!     `<state_dir>/orca.db`.

use anyhow::{Context, Result};
use std::path::PathBuf;

use super::consts;

/// Environment variable naming the orca state directory root. When set it
/// overrides `$HOME/.orca` for EVERY path this module resolves.
pub const ENV_ORCA_HOME: &str = "ORCA_HOME";

/// Environment variable naming an explicit DB file path, overriding the
/// `<state_dir>/orca.db` default. Used for test isolation and unusual layouts.
pub const ENV_ORCA_DB_PATH: &str = "ORCA_DB_PATH";

/// Resolve orca's state dir: `$ORCA_HOME` if set, else `$HOME/.orca`.
/// Returns `None` only when neither env var is set (sealed CI, sandboxes).
/// Prefer [`state_dir`] when you want an error instead of `None`.
pub fn orca_home() -> Option<PathBuf> {
    std::env::var_os(ENV_ORCA_HOME)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(consts::APP_STATE_DIR)))
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

        // Without $ORCA_HOME we fall back to $HOME/.orca.
        unsafe {
            std::env::remove_var(ENV_ORCA_HOME);
            std::env::remove_var(ENV_ORCA_DB_PATH);
        }
        assert_eq!(orca_home().unwrap(), home.join(consts::APP_STATE_DIR));
    }
}
