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
}
