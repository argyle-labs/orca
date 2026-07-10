//! `system.update`'s URL-fetch path — when a peer has a `dev_source` URL
//! configured, `system.update` fetches the binary from that URL instead
//! of GitHub releases.
//!
//! Misnamed historically (`dev_source` / `apply_update_dev`): from the
//! system's POV this is just "fetch from an arbitrary URL," a system
//! primitive used by every peer that configures one. The actual dev
//! tooling that *serves* such binaries lives in the `dev` crate
//! (`dev::serve` runs from a developer's checkout).
//!
//! A future rename (e.g. `dev_source` → `update_source`) is captured in
//! the consolidation followups but deferred to keep this pass mechanical.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::update::{current_binary_path, require_sha256_nonempty, verify_sha256};

fn dev_source_path() -> Option<PathBuf> {
    Some(files::ops::orca_home()?.join("dev-source"))
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
    let url = utils::url::join(source_url, "version.json");
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
    let client = utils::http::Client::new();

    let info: DevVersionInfo = client
        .get(utils::url::join(source_url, "version.json"))
        .send()
        .await
        .context("dev-source version check failed")?
        .json()
        .context("dev-source returned invalid version.json")?;
    require_sha256_nonempty(&info.sha256)?;

    const MAX: usize = 128 * 1024 * 1024;
    println!("[orca] downloading dev build from {source_url}...");
    let resp = client
        .get(utils::url::join(source_url, "binary"))
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

    // Unraid persists binaries in appdata (orca-writeable). No sudo needed.
    // See [[project-unraid-persistence-via-appdata]].
    #[cfg(target_os = "linux")]
    if crate::update::is_unraid() {
        let persist_dir = std::path::Path::new("/mnt/user/appdata/orca/bin");
        let persist_bin = persist_dir.join("orca");
        std::fs::create_dir_all(persist_dir)
            .with_context(|| format!("create unraid appdata dir {}", persist_dir.display()))?;
        std::fs::copy(&current, &persist_bin)
            .with_context(|| format!("mirror dev binary to {}", persist_bin.display()))?;
    }

    // Write the pending_restart marker + schedule supervisor restart, same
    // as the GitHub-release path. Without this, the binary swap lands on
    // disk but the daemon keeps running the old in-memory bytes — visible
    // to operators as "Apply does nothing." `info.sha256` doubles as the
    // marker target since dev builds don't carry a semver tag.
    println!("[orca] dev build applied — scheduling restart");
    crate::update::write_pending_restart_marker(&info.sha256);
    let method = crate::update::schedule_self_restart();
    println!("[orca] restart method: {method}");
    Ok(())
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
}
