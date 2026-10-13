//! The crate's one rule for tests that mutate process env: mark the test
//! `#[serial_test::serial(env)]` and hold [`snapshot`] for its whole body, so
//! every var it sets or removes (including an ambient `ORCA_HOME`) is put back
//! before the next test runs.

use std::ffi::OsString;

pub(crate) struct EnvSnapshot(Vec<(OsString, OsString)>);

#[must_use]
pub(crate) fn snapshot() -> EnvSnapshot {
    EnvSnapshot(std::env::vars_os().collect())
}

impl Drop for EnvSnapshot {
    fn drop(&mut self) {
        // SAFETY: sound under nextest's process-per-test; under threaded cargo test, env readers outside serial(env) can still race.
        unsafe {
            for (k, _) in std::env::vars_os() {
                if !self.0.iter().any(|(sk, _)| *sk == k) {
                    std::env::remove_var(k);
                }
            }
            for (k, v) in &self.0 {
                std::env::set_var(k, v);
            }
        }
    }
}
