//! Dev mode supervisor — clones the orca git repo on demand and runs
//! `cargo watch -x 'run -- daemon'` so a developer's edits hot-reload the
//! local daemon. Driven by `orca dev enable / disable / sync` CLI verbs.
//!
//! Relocated 2026-06-01 from `system::dev`. The fleet-facing URL-fetch
//! path (peer fetches binary from a configured URL) stays in system —
//! that's a system primitive, not a dev concern.

use anyhow::{Context, Result};
use files::ops::chmod_dir_owner_only;
use std::path::PathBuf;
use std::process::Command;

const DEV_REPO_SUBDIR: &str = "dev/orca";

fn dev_repo_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join(DEV_REPO_SUBDIR))
}

fn dev_pid_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join("dev.pid"))
}

/// Find `cargo` for `dev_enable` — daemon-inherited PATH typically lacks
/// `~/.cargo/bin` because rustup's env hook only runs in interactive shells.
fn resolve_cargo_bin() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("CARGO") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let p = PathBuf::from(home).join("bin").join("cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".cargo/bin/cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("cargo");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    for candidate in [
        "/var/lib/orca/.cargo/bin/cargo",
        "/home/orca/.cargo/bin/cargo",
        "/root/.cargo/bin/cargo",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn read_dev_pid() -> Option<u32> {
    std::fs::read_to_string(dev_pid_path()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_dev_pid(pid: u32) -> Result<()> {
    let path = dev_pid_path().context("no ORCA_HOME or HOME set")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, format!("{pid}\n"))?;
    Ok(())
}

fn clear_dev_pid() {
    if let Some(p) = dev_pid_path() {
        _ = std::fs::remove_file(p);
    }
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct DevEnableResult {
    pub repo_path: String,
    pub cloned: bool,
    pub daemon_parked: bool,
}

pub fn cmd_dev_enable(github_token: &str) -> Result<DevEnableResult> {
    use contract::config::APP_REPO_URL;

    let repo = dev_repo_path().context("no ORCA_HOME or HOME")?;

    if let Ok(Some(s)) = utils::state::read()
        && matches!(s.mode, utils::state::DaemonMode::Dev)
        && pid_alive(s.daemon_pid)
    {
        return Ok(DevEnableResult {
            repo_path: repo.to_string_lossy().into(),
            cloned: false,
            daemon_parked: false,
        });
    }

    if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        let daemon_state = utils::state::read()?;
        let daemon_parked = daemon_state
            .as_ref()
            .map(|s| {
                s.mode == utils::state::DaemonMode::Parked
                    || s.mode == utils::state::DaemonMode::Dev
            })
            .unwrap_or(false);
        return Ok(DevEnableResult {
            repo_path: repo.to_string_lossy().into(),
            cloned: false,
            daemon_parked,
        });
    }

    let cloned = if !repo.exists() {
        if let Some(parent) = repo.parent() {
            std::fs::create_dir_all(parent)?;
            chmod_dir_owner_only(parent)
                .with_context(|| format!("chmod 0700 on dev dir {}", parent.display()))?;
        }
        let clone_url = if github_token.is_empty() {
            APP_REPO_URL.to_string()
        } else if let Some(rest) = APP_REPO_URL.strip_prefix("https://") {
            format!("https://x-access-token:{github_token}@{rest}")
        } else {
            APP_REPO_URL.to_string()
        };
        let status = Command::new("git")
            .args([
                "clone",
                "--depth=1",
                &clone_url,
                repo.to_str().unwrap_or("."),
            ])
            .status()?;
        anyhow::ensure!(
            status.success(),
            "git clone failed (private repo — ensure `github_token` secret is set on this host)"
        );
        true
    } else {
        false
    };

    let daemon_parked = match utils::state::read()? {
        Some(s) if s.mode == utils::state::DaemonMode::Daemon => {
            Command::new("kill")
                .args(["-USR1", &s.daemon_pid.to_string()])
                .status()?;
            wait_for_park(s.daemon_pid)?;
            true
        }
        _ => false,
    };

    let cargo_bin = resolve_cargo_bin()
        .context("locate cargo binary (install rustup and ensure ~/.cargo/bin is reachable)")?;
    let cargo_dir = cargo_bin.parent().unwrap_or(std::path::Path::new("/"));
    let augmented_path = match std::env::var_os("PATH") {
        Some(p) => {
            let mut paths = vec![cargo_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&p));
            std::env::join_paths(paths).context("join PATH")?
        }
        None => cargo_dir.as_os_str().to_owned(),
    };
    // Child is intentionally dropped without wait/kill: std::process::Child does
    // NOT kill on drop, so the cargo-watch process outlives this call by design.
    // Lifecycle is owned via the PID file written below; teardown happens via
    // explicit `kill` in the dev disable path, not Drop.
    let child = Command::new(&cargo_bin)
        .args(["watch", "-x", "run -- daemon"])
        .current_dir(&repo)
        .env("PATH", &augmented_path)
        .env("ORCA_DEV_PARENT_PID", "0")
        // Dev is a STATE, signalled by this env var (see `update::is_dev`).
        // The hot-reloaded daemon inherits it, so it reports mode=dev and the
        // updater won't pull a GitHub release over the local build.
        .env("ORCA_DEV", "1")
        .spawn()?;

    let watch_pid = child.id();
    write_dev_pid(watch_pid)?;

    if daemon_parked && let Ok(Some(mut s)) = utils::state::read() {
        s.active_pid = watch_pid;
        _ = utils::state::write(&s);
    }

    Ok(DevEnableResult {
        repo_path: repo.to_string_lossy().into(),
        cloned,
        daemon_parked,
    })
}

fn wait_for_park(daemon_pid: u32) -> Result<()> {
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(Some(s)) = utils::state::read() {
            if s.daemon_pid == daemon_pid && s.mode == utils::state::DaemonMode::Parked {
                return Ok(());
            }
            if s.daemon_pid != daemon_pid {
                return Ok(());
            }
        }
        if !pid_alive(daemon_pid) {
            return Ok(());
        }
    }
    anyhow::bail!("daemon did not park within 5 s")
}

pub struct DevDisableResult {
    pub dev_process_stopped: bool,
    pub daemon_reclaimed: bool,
}

pub fn cmd_dev_disable() -> Result<DevDisableResult> {
    let dev_process_stopped = if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        clear_dev_pid();
        true
    } else {
        clear_dev_pid();
        false
    };

    let daemon_reclaimed = match utils::state::read()? {
        Some(s) if s.mode != utils::state::DaemonMode::Daemon => Command::new("kill")
            .args(["-USR2", &s.daemon_pid.to_string()])
            .status()
            .map(|st| st.success())
            .unwrap_or(false),
        _ => false,
    };

    Ok(DevDisableResult {
        dev_process_stopped,
        daemon_reclaimed,
    })
}

pub struct DevSyncResult {
    pub commits_pulled: u32,
    pub already_up_to_date: bool,
    pub detail: String,
}

pub fn cmd_dev_sync() -> Result<DevSyncResult> {
    let repo = dev_repo_path().context("no ORCA_HOME or HOME")?;
    anyhow::ensure!(
        repo.exists(),
        "dev repo not found at {} — run `orca dev enable` first",
        repo.display()
    );

    let out = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(&repo)
        .output()?;

    let detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let combined = if stderr.is_empty() {
        detail.clone()
    } else {
        format!("{detail}\n{stderr}")
    };

    anyhow::ensure!(out.status.success(), "git pull failed: {combined}");

    let already_up_to_date = detail.contains("Already up to date");
    let commits_pulled = if already_up_to_date {
        0
    } else {
        detail.lines().filter(|l| l.starts_with("   ")).count() as u32
    };

    Ok(DevSyncResult {
        commits_pulled,
        already_up_to_date,
        detail: combined,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every test that mutates process-global env vars. All the
    /// path/pid helpers read `ORCA_HOME`/`HOME`/`CARGO*`/`PATH`, which are
    /// shared across the test binary's threads, so they must not run
    /// concurrently. Held for the whole body of each env-mutating test.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Snapshot + restore of the env vars these helpers depend on. Restoring on
    /// drop keeps tests hermetic even if one panics mid-body.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        const VARS: [&'static str; 5] = ["ORCA_HOME", "HOME", "CARGO", "CARGO_HOME", "PATH"];

        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = Self::VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            let g = Self { _lock: lock, saved };
            for k in Self::VARS {
                g.clear(k);
            }
            g
        }

        fn set(&self, key: &str, val: impl AsRef<std::ffi::OsStr>) {
            // Safety: single-threaded within the ENV_LOCK critical section.
            unsafe { std::env::set_var(key, val) };
        }

        fn clear(&self, key: &str) {
            // Safety: single-threaded within the ENV_LOCK critical section.
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    #[test]
    fn dev_repo_parent_is_chmoded_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let dev_dir = dir.path().join("dev");
        std::fs::create_dir_all(&dev_dir).unwrap();
        chmod_dir_owner_only(&dev_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&dev_dir).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700, "dev dir should be 0700, got {mode:o}");
        }
    }

    #[test]
    fn dev_repo_path_uses_orca_home_and_subdir() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        let repo = dev_repo_path().expect("repo path with ORCA_HOME set");
        assert_eq!(repo, home.path().join("dev").join("orca"));
        assert!(repo.ends_with("dev/orca"));
    }

    #[test]
    fn dev_repo_path_falls_back_to_home_dot_orca() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());

        let repo = dev_repo_path().expect("repo path with HOME set");
        assert_eq!(repo, home.path().join(".orca").join("dev").join("orca"));
    }

    #[test]
    fn dev_pid_path_is_dev_pid_under_orca_home() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        let pid = dev_pid_path().expect("pid path with ORCA_HOME set");
        assert_eq!(pid, home.path().join("dev.pid"));
    }

    #[test]
    fn path_helpers_return_none_without_home() {
        let _env = EnvGuard::new();
        // Both ORCA_HOME and HOME cleared by the guard.
        assert!(dev_repo_path().is_none());
        assert!(dev_pid_path().is_none());
    }

    #[test]
    fn write_then_read_dev_pid_roundtrips() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());

        write_dev_pid(4242).unwrap();
        // Parent dir must exist and file must carry a trailing newline.
        let raw = std::fs::read_to_string(home.path().join("dev.pid")).unwrap();
        assert_eq!(raw, "4242\n");
        assert_eq!(read_dev_pid(), Some(4242));
    }

    #[test]
    fn write_dev_pid_creates_missing_parent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        // Point ORCA_HOME at a not-yet-created nested dir.
        let nested = home.path().join("a").join("b");
        env.set("ORCA_HOME", &nested);

        write_dev_pid(7).unwrap();
        assert!(nested.join("dev.pid").is_file());
        assert_eq!(read_dev_pid(), Some(7));
    }

    #[test]
    fn write_dev_pid_errors_without_home() {
        let _env = EnvGuard::new();
        let err = write_dev_pid(1).unwrap_err();
        assert!(err.to_string().contains("ORCA_HOME") || err.to_string().contains("HOME"));
    }

    #[test]
    fn read_dev_pid_none_when_file_absent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        assert_eq!(read_dev_pid(), None);
    }

    #[test]
    fn read_dev_pid_none_on_garbage_contents() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "not-a-pid\n").unwrap();
        assert_eq!(read_dev_pid(), None);
    }

    #[test]
    fn read_dev_pid_trims_whitespace() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "  915  \n").unwrap();
        assert_eq!(read_dev_pid(), Some(915));
    }

    #[test]
    fn clear_dev_pid_removes_file_and_is_idempotent() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let pid_file = home.path().join("dev.pid");
        std::fs::write(&pid_file, "5\n").unwrap();
        assert!(pid_file.exists());

        clear_dev_pid();
        assert!(!pid_file.exists());
        // Second call on an already-absent file must not panic or error.
        clear_dev_pid();
        assert!(!pid_file.exists());
    }

    #[test]
    fn resolve_cargo_bin_prefers_valid_cargo_env() {
        let env = EnvGuard::new();
        // A real regular file standing in for the cargo binary.
        let dir = tempfile::tempdir().unwrap();
        let fake_cargo = dir.path().join("cargo");
        std::fs::write(&fake_cargo, b"#!/bin/sh\n").unwrap();
        env.set("CARGO", &fake_cargo);

        assert_eq!(resolve_cargo_bin(), Some(fake_cargo));
    }

    #[test]
    fn resolve_cargo_bin_ignores_nonfile_cargo_and_uses_cargo_home() {
        let env = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        // CARGO points at a directory (not a file) -> skipped.
        env.set("CARGO", dir.path());
        // CARGO_HOME/bin/cargo is a real file -> chosen.
        let cargo_home = tempfile::tempdir().unwrap();
        let bin = cargo_home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let cargo = bin.join("cargo");
        std::fs::write(&cargo, b"x").unwrap();
        env.set("CARGO_HOME", cargo_home.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn resolve_cargo_bin_finds_cargo_on_path() {
        let env = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("cargo");
        std::fs::write(&cargo, b"x").unwrap();
        env.set("PATH", dir.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn pid_alive_true_for_current_process() {
        // pid_alive shells out to `kill` resolved via PATH; hold ENV_LOCK so a
        // concurrent EnvGuard (which clears PATH) can't run and make `kill`
        // unresolvable mid-test.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_false_for_unused_pid() {
        // Also resolves `kill` via PATH — serialize under ENV_LOCK so a
        // concurrent EnvGuard clearing PATH can't turn the "not found" IO error
        // into a misleading pass (it already returns false, but keep it honest).
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Very high PID is not in use on any realistic system.
        assert!(!pid_alive(4_294_967_294));
    }

    #[test]
    fn cmd_dev_sync_errors_when_repo_missing() {
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        // ORCA_HOME set but no dev/orca repo cloned under it.
        env.set("ORCA_HOME", home.path());
        // DevSyncResult has no Debug impl, so `.err()` rather than `unwrap_err()`.
        let err = cmd_dev_sync().err().expect("expected repo-missing error");
        assert!(
            err.to_string().contains("dev repo not found"),
            "unexpected error: {err}"
        );
    }

    /// Snapshot + clear the `GIT_*` vars git subprocesses inherit, restoring on
    /// drop. Under a pre-commit hook `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`
    /// point at the outer repo and would corrupt the temp repo these tests drive.
    struct GitEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl GitEnvGuard {
        const VARS: [&'static str; 3] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"];
        fn new() -> Self {
            let saved = Self::VARS
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            for k in Self::VARS {
                // Safety: caller holds ENV_LOCK via EnvGuard.
                unsafe { std::env::remove_var(k) };
            }
            Self { saved }
        }
    }
    impl Drop for GitEnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    /// Runs `git args...` in `dir` with deterministic identity, asserting success.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git spawns");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Clone an origin repo with one initial commit into `ORCA_HOME/dev/orca`.
    /// Returns (origin_dir_tempdir_kept_alive_by_caller, origin_path).
    fn setup_synced_repo(home: &std::path::Path) -> (tempfile::TempDir, PathBuf) {
        let origin_td = tempfile::tempdir().unwrap();
        let origin = origin_td.path().to_path_buf();
        git_in(&origin, &["init", "-q"]);
        git_in(&origin, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let dev_orca = home.join("dev").join("orca");
        std::fs::create_dir_all(dev_orca.parent().unwrap()).unwrap();
        git_in(
            home,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                dev_orca.to_str().unwrap(),
            ],
        );
        (origin_td, origin)
    }

    #[test]
    fn cmd_dev_sync_already_up_to_date_pulls_zero_commits() {
        let env = EnvGuard::new();
        // Restore a PATH so `git` resolves; the guard cleared it.
        env.set("PATH", "/usr/bin:/bin:/usr/local/bin");
        let _git = GitEnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let _origin = setup_synced_repo(home.path());

        let r = cmd_dev_sync().err();
        // Should be Ok, not an error.
        assert!(r.is_none(), "sync should succeed, got {r:?}");
        let r = cmd_dev_sync().expect("second sync Ok");
        assert!(r.already_up_to_date, "no new commits → already up to date");
        assert_eq!(r.commits_pulled, 0);
        assert!(
            r.detail.contains("up to date") || r.detail.contains("up-to-date"),
            "detail should note up-to-date, got {:?}",
            r.detail
        );
    }

    #[test]
    fn cmd_dev_sync_fast_forwards_new_commit() {
        let env = EnvGuard::new();
        env.set("PATH", "/usr/bin:/bin:/usr/local/bin");
        let _git = GitEnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let (_origin_td, origin) = setup_synced_repo(home.path());

        // Add a new commit on origin so the clone can fast-forward.
        std::fs::write(origin.join("f.txt"), "hello\n").unwrap();
        git_in(&origin, &["add", "f.txt"]);
        git_in(&origin, &["commit", "-q", "-m", "add f"]);

        let r = cmd_dev_sync().expect("fast-forward sync Ok");
        assert!(!r.already_up_to_date, "a new commit was pulled");
        assert!(
            r.detail.contains("Fast-forward") || r.detail.contains("Updating"),
            "detail should reflect a fast-forward, got {:?}",
            r.detail
        );
    }

    #[test]
    fn cmd_dev_sync_errors_without_home() {
        let _env = EnvGuard::new();
        // Neither ORCA_HOME nor HOME → dev_repo_path is None → context error.
        // DevSyncResult has no Debug impl, so `.err()` rather than `unwrap_err()`.
        let err = cmd_dev_sync().err().expect("expected no-home error");
        assert!(
            err.to_string().contains("ORCA_HOME") || err.to_string().contains("HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cmd_dev_disable_with_no_pid_and_no_state_is_clean_noop() {
        // Holds ENV_LOCK via EnvGuard: cmd_dev_disable may shell out to `kill`
        // only when a live pid/state exists — here there is neither, so no
        // process is signalled. state::read honors ORCA_HOME → Ok(None).
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        // No dev.pid file and no daemon state file under this ORCA_HOME.
        let r = cmd_dev_disable().expect("disable with nothing running is Ok");
        assert!(!r.dev_process_stopped, "no pid file → nothing to stop");
        assert!(!r.daemon_reclaimed, "no daemon state → nothing to reclaim");
    }

    #[test]
    fn cmd_dev_disable_clears_stale_pid_file_for_dead_pid() {
        // A pid file pointing at a dead pid: dev_process_stopped stays false
        // (pid not alive) but the stale file is cleared as a side effect.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        std::fs::write(home.path().join("dev.pid"), "4294967294\n").unwrap();
        let r = cmd_dev_disable().expect("disable Ok");
        assert!(!r.dev_process_stopped, "dead pid → not stopped");
        assert!(
            !home.path().join("dev.pid").exists(),
            "stale pid file must be cleared"
        );
    }

    #[test]
    fn resolve_cargo_bin_uses_home_dot_cargo_when_env_absent() {
        let env = EnvGuard::new();
        // CARGO / CARGO_HOME / PATH are all cleared by the guard, so resolution
        // falls through to the HOME/.cargo/bin/cargo branch.
        let home = tempfile::tempdir().unwrap();
        let cargo = home.path().join(".cargo").join("bin").join("cargo");
        std::fs::create_dir_all(cargo.parent().unwrap()).unwrap();
        std::fs::write(&cargo, b"x").unwrap();
        env.set("HOME", home.path());

        assert_eq!(resolve_cargo_bin(), Some(cargo));
    }

    #[test]
    fn resolve_cargo_bin_returns_none_when_nothing_resolves() {
        let env = EnvGuard::new();
        // All of CARGO/CARGO_HOME/HOME/PATH cleared by the guard, and the
        // hardcoded system fallbacks (/var/lib/orca, /root/.cargo, …) don't
        // exist in the test environment → no cargo can be located.
        let home = tempfile::tempdir().unwrap();
        // HOME points at an empty dir with no .cargo/bin/cargo.
        env.set("HOME", home.path());
        assert_eq!(resolve_cargo_bin(), None);
    }

    /// Build a DaemonState with the given mode and pids for exercising the
    /// early-return branches of `cmd_dev_enable` without spawning cargo-watch.
    fn state_with(mode: utils::state::DaemonMode, daemon_pid: u32) -> utils::state::DaemonState {
        utils::state::DaemonState {
            daemon_pid,
            active_pid: daemon_pid,
            port: 12000,
            mode,
            binary: "/usr/local/bin/orca".to_string(),
            version: "0.1.0".to_string(),
            started_at: utils::time::now(),
        }
    }

    #[test]
    fn cmd_dev_enable_returns_early_when_dev_daemon_already_alive() {
        // State says mode=Dev with a live daemon_pid (this process): the first
        // early-return fires, so nothing is cloned or parked and no cargo-watch
        // is spawned.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        utils::state::write(&state_with(utils::state::DaemonMode::Dev, me)).unwrap();

        let r = cmd_dev_enable("").expect("enable is Ok on already-dev state");
        assert!(!r.cloned, "existing repo/state → not cloned");
        assert!(!r.daemon_parked, "already dev → nothing parked");
        assert!(
            r.repo_path.ends_with("dev/orca"),
            "repo_path should point at the dev repo, got {}",
            r.repo_path
        );
    }

    #[test]
    fn cmd_dev_enable_returns_early_on_live_dev_pid_with_parked_daemon() {
        // No live Dev daemon in state (mode=Parked), but a live dev.pid file
        // exists → second early-return fires and reports the daemon as parked.
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        // Daemon state is Parked (not Dev), so the first branch is skipped.
        utils::state::write(&state_with(utils::state::DaemonMode::Parked, me)).unwrap();
        // A live dev-process pid triggers the second early return.
        std::fs::write(home.path().join("dev.pid"), format!("{me}\n")).unwrap();

        let r = cmd_dev_enable("").expect("enable Ok on live dev pid");
        assert!(!r.cloned);
        assert!(r.daemon_parked, "parked state → daemon_parked true");
    }

    #[test]
    fn cmd_dev_enable_live_dev_pid_reports_unparked_for_plain_daemon() {
        // Live dev.pid but state mode=Daemon → second branch reports
        // daemon_parked=false (daemon still owns the port).
        let env = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("ORCA_HOME", home.path());
        let me = std::process::id();
        utils::state::write(&state_with(utils::state::DaemonMode::Daemon, me)).unwrap();
        std::fs::write(home.path().join("dev.pid"), format!("{me}\n")).unwrap();

        let r = cmd_dev_enable("").expect("enable Ok");
        assert!(!r.cloned);
        assert!(!r.daemon_parked, "plain daemon → not parked");
    }

    #[test]
    fn dev_enable_result_fields_are_addressable() {
        // Guards the public result struct shape used by the CLI layer.
        let r = DevEnableResult {
            repo_path: "/tmp/x".into(),
            cloned: true,
            daemon_parked: false,
        };
        assert_eq!(r.repo_path, "/tmp/x");
        assert!(r.cloned);
        assert!(!r.daemon_parked);

        let d = DevDisableResult {
            dev_process_stopped: true,
            daemon_reclaimed: false,
        };
        assert!(d.dev_process_stopped);
        assert!(!d.daemon_reclaimed);

        let s = DevSyncResult {
            commits_pulled: 3,
            already_up_to_date: false,
            detail: "pulled".into(),
        };
        assert_eq!(s.commits_pulled, 3);
        assert!(!s.already_up_to_date);
        assert_eq!(s.detail, "pulled");
    }
}
