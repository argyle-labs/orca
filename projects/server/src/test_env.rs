//! Serializes lib tests that repoint the process-global `ORCA_HOME` /
//! `ORCA_DB_PATH`. `cargo test` runs tests on parallel threads of one process,
//! so every such test across the crate must hold this one guard.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

const VARS: [&str; 2] = ["ORCA_HOME", "ORCA_DB_PATH"];

static LOCK: Mutex<()> = Mutex::new(());

/// Holds the crate-wide env lock and restores the vars' prior values on drop,
/// so an outer `ORCA_HOME` (e.g. a scratch dir for the run) survives the test.
pub(crate) struct OrcaEnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

pub(crate) fn lock_orca_env() -> OrcaEnvGuard {
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
    OrcaEnvGuard { saved, _lock: lock }
}

impl Drop for OrcaEnvGuard {
    fn drop(&mut self) {
        for (var, val) in &self.saved {
            // SAFETY: every test that mutates these vars holds `LOCK`.
            unsafe {
                match val {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}
