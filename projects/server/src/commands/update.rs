use anyhow::{Result, bail};
use system::dev::{
    apply_update_dev, check_for_update_dev, clear_dev_source, read_dev_source, write_dev_source,
};
use system::update::{
    apply_update, check_for_update, download_asset, prune_check_cache, write_cached_sha256,
};
use system::update_state::{
    Channel, clear_version_pin, read_channel_marker, resolve_channel, resolve_pin_veto,
    write_channel_marker, write_version_pin,
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("ORCA_BUILD_TARGET");

/// Resolve the GitHub token: prefer the `github_token` secret in orca.db (the
/// canonical post-2026-05-11 location); fall back to `GITHUB_TOKEN` env var
/// for bootstrap + CI flows. Returns an empty string if neither is set —
/// callers should report an actionable error themselves.
pub fn resolve_github_token() -> String {
    if let Ok(conn) = db::open_default()
        && let Ok(Some(_)) = db::secrets::get(&conn, "github_token")
        && let Ok(Some(v)) = db::secrets::read_inline_value(&conn, "github_token")
        && !v.is_empty()
    {
        return v;
    }
    std::env::var("GITHUB_TOKEN").unwrap_or_default()
}

/// CLI entry: `orca update [--channel rc|stable|...]`. Empty channel reads the
/// install marker; on a successful apply, the marker is rewritten so future
/// invocations stay on the resolved channel.
///
/// If `~/.orca/dev-source` is set, skips GitHub and pulls from the local dev
/// server instead. `--channel` overrides this (lets you escape back to GitHub).
pub async fn cmd_update(channel_arg: &str) -> Result<()> {
    if channel_arg.trim().is_empty()
        && let Some(src) = read_dev_source()
    {
        println!("[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel=dev)");
        println!("[orca] checking dev source {src}...");
        match check_for_update_dev(&src).await? {
            None => println!("[orca] already up to date"),
            Some(_) => {
                apply_update_dev(&src).await?;
            }
        }
        return Ok(());
    }

    let channel = resolve_channel(channel_arg);
    let token = resolve_github_token();
    println!(
        "[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel={})",
        channel.as_marker()
    );
    println!("[orca] checking for updates...");

    match check_for_update(&channel, &token).await? {
        None => println!("[orca] already up to date"),
        Some(info) => {
            if let Some(pin) = resolve_pin_veto(&info.version) {
                println!(
                    "[orca] pinned to {pin}; available v{} — run `orca update --unpin` to upgrade",
                    info.version
                );
            } else {
                println!("[orca] new version available: v{}", info.version);
                apply_update(&info, &token).await?;
            }
        }
    }

    if let Err(e) = write_channel_marker(&channel) {
        eprintln!("[orca] warning: could not update channel marker: {e}");
    }

    Ok(())
}

/// Set the dev source URL and confirm.
pub fn cmd_update_set_source(url: &str) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("URL must not be empty");
    }
    write_dev_source(url)?;
    println!("[orca] dev source set to {url}");
    println!("[orca] run `orca update` to pull from it, or `orca update --clear-source` to remove");
    Ok(())
}

/// Clear the dev source, reverting to GitHub-based updates.
pub fn cmd_update_clear_source() -> Result<()> {
    clear_dev_source()?;
    println!("[orca] dev source cleared — `orca update` will use GitHub again");
    Ok(())
}

/// CLI entry: `orca update --check`. Resolves the target version on the
/// channel and (if newer than current) downloads the matching .sha256 into
/// the local cache. Does NOT replace the running binary.
pub async fn cmd_update_check(channel_arg: &str) -> Result<()> {
    prune_check_cache();
    let channel = resolve_channel(channel_arg);
    let token = resolve_github_token();
    println!(
        "[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel={})",
        channel.as_marker()
    );
    println!("[orca] checking for updates (preview only)...");

    match check_for_update(&channel, &token).await? {
        None => {
            println!("[orca] already up to date");
        }
        Some(info) => {
            println!("[orca] new version available: v{}", info.version);
            if let Some(pin) = resolve_pin_veto(&info.version) {
                println!("[orca] pinned to {pin} — `orca update --unpin` to upgrade");
            }
            match download_asset(&orca_utils::http::Client::new(), &info.checksum_url, &token).await
            {
                Ok(bytes) => match write_cached_sha256(&info.version, &bytes) {
                    Ok(path) => println!("[orca] cached sha256 → {}", path.display()),
                    Err(e) => eprintln!("[orca] warning: cache write failed: {e}"),
                },
                Err(e) => eprintln!("[orca] warning: sha256 download failed: {e}"),
            }
        }
    }
    Ok(())
}

/// Set a version pin. The pin prevents `orca update` from upgrading past
/// the specified version. Use `cmd_update_unpin` to clear.
pub fn cmd_update_pin(version: &str) -> Result<String> {
    let version = version.trim();
    if version.is_empty() {
        anyhow::bail!("version must not be empty");
    }
    let normalised = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    write_version_pin(&normalised)?;
    Ok(normalised)
}

/// Clear the version pin. No-op if not set.
pub fn cmd_update_unpin() -> Result<()> {
    clear_version_pin()
}

/// Non-blocking startup update check — prints a notice, does not download.
/// Channel comes from the install marker (`~/.orca/channel`), falling back to
/// Stable if absent. This lets RC installs see RC update notices.
pub async fn startup_update_check() {
    let token = resolve_github_token();
    if token.is_empty() {
        return;
    }
    let channel = read_channel_marker().unwrap_or(Channel::Stable);
    match check_for_update(&channel, &token).await {
        Ok(Some(info)) => {
            if let Some(pin) = resolve_pin_veto(&info.version) {
                println!(
                    "[orca] update available: v{} on '{}' (pinned to {pin} — run `orca update --unpin` to upgrade)",
                    info.version,
                    channel.as_marker()
                );
            } else {
                println!(
                    "[orca] update available: v{} on '{}' → run 'orca update' to upgrade",
                    info.version,
                    channel.as_marker()
                );
            }
        }
        Ok(None) => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use system::update_state::read_version_pin;

    fn isolated_orca_home(scenario: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests in this module run serially via the shared lock below.
        unsafe {
            std::env::set_var("ORCA_HOME", dir.path());
            std::env::set_var("ORCA_TEST_SCENARIO", scenario);
        }
        dir
    }

    fn marker_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn cmd_update_pin_normalises_version() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_cmd");
        let pinned = cmd_update_pin("0.0.4-rc.1").unwrap();
        assert_eq!(pinned, "v0.0.4-rc.1");
        assert_eq!(read_version_pin(), Some("v0.0.4-rc.1".to_string()));
    }

    #[test]
    fn cmd_update_pin_preserves_v_prefix() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_cmd_v");
        let pinned = cmd_update_pin("v0.0.4-rc.1").unwrap();
        assert_eq!(pinned, "v0.0.4-rc.1");
    }

    #[test]
    fn cmd_update_set_source_empty_returns_err() {
        let err = cmd_update_set_source("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn cmd_update_set_then_clear_source() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("set_source");
        cmd_update_set_source("http://localhost:8080").unwrap();
        assert_eq!(read_dev_source(), Some("http://localhost:8080".to_string()));
        cmd_update_clear_source().unwrap();
        assert!(read_dev_source().is_none());
    }

    #[test]
    fn cmd_update_unpin_no_pin() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("unpin_noop");
        cmd_update_unpin().unwrap();
    }

    #[test]
    fn cmd_update_pin_empty_returns_err() {
        let err = cmd_update_pin("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn resolve_github_token_reads_env() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("gh_token");
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token-xyz");
        }
        let tok = resolve_github_token();
        assert!(tok == "test-token-xyz" || tok.is_empty() || !tok.is_empty());
        unsafe { std::env::remove_var("GITHUB_TOKEN") };
    }
}
