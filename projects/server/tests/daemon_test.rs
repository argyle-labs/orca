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
    use orca_utils::state::{self, DaemonMode};

    const TEST_PORT: u16 = 19998;
    const TIMEOUT: Duration = Duration::from_secs(15);
    const POLL: Duration = Duration::from_millis(150);

    fn send_signal(pid: u32, sig: &str) {
        let status = std::process::Command::new("kill")
            .args([&format!("-{sig}"), &pid.to_string()])
            .status()
            .expect("kill command failed");
        assert!(status.success(), "kill -{sig} {pid} failed");
    }

    fn wait_for_mode(state_path: &Path, target: DaemonMode) -> u32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for mode={target:?}");
            }
            std::thread::sleep(POLL);
            if let Ok(Some(s)) = state::read_from(state_path) {
                if s.mode == target {
                    return s.daemon_pid;
                }
            }
        }
    }

    fn wait_for_file_gone(path: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if !path.exists() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for state file removal");
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn daemon_sigusr1_parks_sigusr2_reclaims_sigterm_exits() {
        let tmpdir = tempdir().expect("tempdir");
        let home = tmpdir.path().to_str().unwrap();
        let state_path = tmpdir.path().join(".orca/state.json");

        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_orca"))
            .env("HOME", home)
            .args(["daemon", "start", "--port", &TEST_PORT.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn orca daemon");

        // Wait for mode=Daemon (server bound and state written)
        let pid = wait_for_mode(&state_path, DaemonMode::Daemon);

        // SIGUSR1 → park
        send_signal(pid, "USR1");
        wait_for_mode(&state_path, DaemonMode::Parked);

        // SIGUSR2 → reclaim
        send_signal(pid, "USR2");
        wait_for_mode(&state_path, DaemonMode::Daemon);

        // SIGTERM → clean shutdown (state file removed)
        send_signal(pid, "TERM");
        wait_for_file_gone(&state_path);

        let _ = child.wait();
    }
}
