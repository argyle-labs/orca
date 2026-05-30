//! Dev mode — local cargo-watch supervisor + dev-source HTTP fetcher.
//!
//! Relocated from `server/src/commands/update.rs` as slice B2c of the
//! server-crate dissolution. Pure CLI thin wrappers (cmd_update_*) stay in
//! `server/src/commands/update.rs`; everything dev-runtime-related lives here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;
use utils::fs::chmod_dir_owner_only;

use crate::update::{
    current_binary_path, require_sha256_nonempty, resolve_github_token, verify_sha256,
};

// ── Dev source (local serve) ──────────────────────────────────────────────────

fn dev_source_path() -> Option<PathBuf> {
    Some(utils::fs::orca_home()?.join("dev-source"))
}

pub fn read_dev_source() -> Option<String> {
    let raw = std::fs::read_to_string(dev_source_path()?).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub fn write_dev_source(url: &str) -> Result<()> {
    let path = dev_source_path().context("no ORCA_HOME or HOME set")?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, format!("{}\n", url.trim()))?;
    Ok(())
}

pub fn clear_dev_source() -> Result<()> {
    if let Some(path) = dev_source_path()
        && path.exists()
    {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DevVersionInfo {
    pub sha256: String,
}

/// Check a local dev-serve endpoint for a newer binary.
/// Returns `Some` if the sha256 on the server differs from the running binary.
pub async fn check_for_update_dev(source_url: &str) -> Result<Option<String>> {
    let url = format!("{}/version.json", source_url.trim_end_matches('/'));
    let client = utils::http::Client::new();
    let info: DevVersionInfo = client
        .get(url)
        .send()
        .await
        .context("dev-source version check failed")?
        .json()
        .context("dev-source returned invalid version.json")?;

    let current = current_binary_path()?;
    let current_sha = utils::hash::sha256_file(&current).unwrap_or_default();
    if info.sha256 == current_sha {
        Ok(None)
    } else {
        Ok(Some(info.sha256))
    }
}

/// Download and apply a binary from a local dev-serve endpoint. Fetches
/// `/version.json` to pin the expected sha256, then sha256-verifies the
/// downloaded bytes before writing — fail-closed, no install without match.
pub async fn apply_update_dev(source_url: &str) -> Result<()> {
    let base = source_url.trim_end_matches('/');
    let client = utils::http::Client::new();

    let info: DevVersionInfo = client
        .get(format!("{base}/version.json"))
        .send()
        .await
        .context("dev-source version check failed")?
        .json()
        .context("dev-source returned invalid version.json")?;
    require_sha256_nonempty(&info.sha256)?;

    const MAX: usize = 128 * 1024 * 1024;
    println!("[orca] downloading dev build from {source_url}...");
    let resp = client
        .get(format!("{base}/binary"))
        .max_body(MAX)
        .timeout(std::time::Duration::from_secs(120))
        .send_bytes()
        .await
        .context("dev binary download failed")?;

    verify_sha256(&resp.body, &info.sha256).map_err(|e| anyhow::anyhow!("dev-source {e}"))?;
    println!("[orca] dev-source checksum OK");

    let current = current_binary_path()?;
    let tmp = current.with_extension("tmp");
    std::fs::write(&tmp, &resp.body).context("failed to write temp binary")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, &current).context("failed to replace binary")?;

    #[cfg(target_os = "linux")]
    if crate::update::is_unraid() {
        let persist_bin = std::path::Path::new("/boot/config/plugins/orca/bin/orca");
        if let Some(parent) = persist_bin.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::copy(&current, persist_bin) {
            tracing::warn!(
                "unraid USB mirror to {} failed: {e:#} — update will not survive reboot",
                persist_bin.display()
            );
        } else {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(persist_bin, std::fs::Permissions::from_mode(0o755));
        }
    }

    println!("[orca] dev build applied — restarting...");
    Ok(())
}

// ── Dev mode (git checkout + cargo watch) ────────────────────────────────────

const DEV_REPO_SUBDIR: &str = "dev/orca";

fn dev_repo_path() -> Option<PathBuf> {
    Some(utils::fs::orca_home()?.join(DEV_REPO_SUBDIR))
}

fn dev_pid_path() -> Option<PathBuf> {
    Some(utils::fs::orca_home()?.join("dev.pid"))
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
    let raw = std::fs::read_to_string(dev_pid_path()?).ok()?;
    raw.trim().parse().ok()
}

fn write_dev_pid(pid: u32) -> Result<()> {
    let path = dev_pid_path().context("no ORCA_HOME or HOME")?;
    std::fs::write(path, format!("{pid}\n"))?;
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

pub fn cmd_dev_enable() -> Result<DevEnableResult> {
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
        let token = resolve_github_token();
        let clone_url = if token.is_empty() {
            APP_REPO_URL.to_string()
        } else if let Some(rest) = APP_REPO_URL.strip_prefix("https://") {
            format!("https://x-access-token:{token}@{rest}")
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
            tokio_block_on_park(s.daemon_pid)?;
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

fn tokio_block_on_park(daemon_pid: u32) -> Result<()> {
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
        "dev repo not found at {} — run `system dev enable` first",
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
    use serial_test::serial;

    fn isolated_orca_home(scenario: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", dir.path());
            std::env::set_var("ORCA_TEST_SCENARIO", scenario);
        }
        dir
    }

    #[test]
    #[serial(env)]
    fn dev_source_round_trips() {
        let _dir = isolated_orca_home("dev_src");
        assert!(read_dev_source().is_none());
        write_dev_source("http://localhost:9999").unwrap();
        assert_eq!(read_dev_source(), Some("http://localhost:9999".to_string()));
        clear_dev_source().unwrap();
        assert!(read_dev_source().is_none());
    }

    #[test]
    #[serial(env)]
    fn dev_source_clear_is_noop_when_absent() {
        let _dir = isolated_orca_home("dev_src_noop");
        clear_dev_source().unwrap();
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
}
