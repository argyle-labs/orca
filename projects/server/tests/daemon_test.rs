/// Integration test for the `orca daemon start` signal loop.
///
/// Spawns the real binary with a temporary HOME so state.json is isolated,
/// then drives SIGUSR1 (park) → SIGUSR2 (reclaim) → SIGTERM (shutdown) and
/// verifies each state transition via the state file.
///
/// Requires the binary to be built before running: `cargo build`.
#[cfg(unix)]
#[cfg(test)]
mod daemon_signal_tests {
    use std::path::Path;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;
    use utils::state::DaemonMode;

    /// HTTP port for the test daemon. Picked well above the orca default
    /// (`APP_REST_HTTP_PORT=12000`) so dev daemons on the workstation don't
    /// collide with the test process.
    const TEST_HTTP_PORT: u16 = 19998;
    /// HTTPS port for the test daemon. Must be distinct from any other
    /// process on the box (including a running real orca daemon on 12443).
    const TEST_HTTPS_PORT: u16 = 19999;
    // Generous: this test spawns a REAL daemon process and drives it via signals.
    // Under `make release` / CI the box runs the whole nextest suite (1400+ tests)
    // in parallel, saturating every core, so the spawned daemon's tokio runtime can
    // be starved and its signal future polled seconds late. A normal park/reclaim is
    // sub-second; the only failure mode this large window allows through is a genuine
    // hang (handler never fires), not load jitter. 15s was too tight and flaked at
    // exactly the deadline under a saturated box.
    const TIMEOUT: Duration = Duration::from_secs(60);
    const POLL: Duration = Duration::from_millis(150);

    /// RAII guard: kills + reaps the spawned daemon on drop, including on a
    /// test panic. Without this, a panic (e.g. a timeout) leaves the daemon
    /// orphaned, still holding the fixed test ports (19998/19999); the NEXT
    /// run's daemon then can't bind and exits within ~1s — turning one flaky
    /// failure into a cascade of port-conflict failures across runs.
    struct DaemonGuard(std::process::Child);

    impl Drop for DaemonGuard {
        fn drop(&mut self) {
            self.0.kill().ok();
            self.0.wait().ok();
        }
    }

    fn send_signal(pid: u32, sig: &str) {
        let status = std::process::Command::new("kill")
            .args([&format!("-{sig}"), &pid.to_string()])
            .status()
            .expect("kill command failed");
        assert!(status.success(), "kill -{sig} {pid} failed");
    }

    fn wait_for_mode(
        child: &mut std::process::Child,
        state_path: &Path,
        target: DaemonMode,
    ) -> u32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            // Fail fast on a genuine regression: if the daemon process has
            // exited, no amount of waiting will reach `target` — surface its
            // exit status now instead of blocking until the timeout.
            if let Ok(Some(status)) = child.try_wait() {
                panic!("daemon exited ({status}) before reaching mode={target:?}");
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for mode={target:?}");
            }
            std::thread::sleep(POLL);
            if let Ok(Some(s)) = utils::state::read_from(state_path)
                && s.mode == target
            {
                return s.daemon_pid;
            }
        }
    }

    fn wait_for_file_gone(path: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if !path.exists() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for state file removal"
            );
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn daemon_sigusr1_parks_sigusr2_reclaims_sigterm_exits() {
        let tmpdir = tempdir().expect("tempdir");
        let home = tmpdir.path().to_str().unwrap();
        let state_path = tmpdir.path().join(".orca/state.json");

        let child = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("HOME", home)
            // Override HTTPS port — the daemon now dual-binds, and the
            // default 12443 collides with any running real daemon on the
            // workstation. ORCA_HTTPS_PORT is the only knob (the test
            // doesn't expose a `--https-port` flag).
            .env("ORCA_HTTPS_PORT", TEST_HTTPS_PORT.to_string())
            .args(["daemon", "--port", &TEST_HTTP_PORT.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn orca daemon");
        // Own the child in a kill-on-drop guard so any panic below (timeout,
        // failed transition) still reaps the daemon instead of orphaning it on
        // the fixed test ports.
        let mut guard = DaemonGuard(child);

        // Wait for mode=Daemon (server bound and state written)
        let pid = wait_for_mode(&mut guard.0, &state_path, DaemonMode::Daemon);

        // SIGUSR1 → park
        send_signal(pid, "USR1");
        wait_for_mode(&mut guard.0, &state_path, DaemonMode::Parked);

        // SIGUSR2 → reclaim
        send_signal(pid, "USR2");
        wait_for_mode(&mut guard.0, &state_path, DaemonMode::Daemon);

        // SIGTERM → clean shutdown (state file removed)
        send_signal(pid, "TERM");
        wait_for_file_gone(&state_path);

        // `guard` drops here, reaping the (now-exited) daemon process.
    }
}
