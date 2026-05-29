//! Update binary swap + GitHub release scanning + sha256 verification.
//!
//! Moved from `server::commands::update` (slice B2b). Owns the
//! high-risk binary-swap codepath: download a release asset from
//! GitHub, verify its checksum, atomically replace the running binary,
//! and schedule a supervisor restart. Channel/pin state is in
//! [`super::update_state`]; dev-mode supervisor + dev-source HTTP
//! fetcher remain in `server::commands::update` (B2c).
//!
//! Also owns [`resolve_github_token`] — the single canonical GitHub PAT
//! resolver shared by the production update path, the dev-source fetcher,
//! and the high-level lifecycle tools. Prefers the `github_token` secret in
//! orca.db; falls back to `$GITHUB_TOKEN` for bootstrap / CI.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;
use utils::config::{APP_NAME, APP_REPO_API_URL};

use crate::update_state::{Channel, is_newer_full};

/// Resolve the GitHub token: prefer the `github_token` secret in orca.db
/// (the canonical post-2026-05-11 location); fall back to `GITHUB_TOKEN` env
/// var for bootstrap + CI flows. Returns an empty string if neither is set —
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

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("ORCA_BUILD_TARGET");
// Current stable as of 2026-05 — check https://docs.github.com/en/rest/about-the-rest-api/api-versions
const GITHUB_API_VERSION: &str = "2022-11-28";

#[derive(Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub asset_url: String,
    pub checksum_url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    url: String, // API asset URL
}

/// Check GitHub for a newer release on the given channel.
/// Stable channel: skips any pre-release tags.
/// Rc/beta/alpha: also accepts pre-releases of that tier and below.
/// Caller supplies the GitHub bearer token (resolved via the secrets service
/// or env fallback).
pub async fn check_for_update(channel: &Channel, token: &str) -> Result<Option<UpdateInfo>> {
    if token.is_empty() {
        bail!("no github token available — set secret 'github_token' or export GITHUB_TOKEN");
    }

    let client = utils::http::Client::new();
    let user_agent = format!("{APP_NAME}/{CURRENT_VERSION}");

    let github_req = |url: String| {
        client
            .get(url)
            .bearer(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("User-Agent", &user_agent)
    };

    // For stable we can use /releases/latest (always returns stable).
    // For pre-release channels we must scan /releases (paginated list).
    let releases: Vec<Release> = if *channel == Channel::Stable {
        let url = format!("{APP_REPO_API_URL}/releases/latest");
        match github_req(url).send().await {
            Ok(resp) => vec![resp.json().context("failed to parse release JSON")?],
            Err(utils::http::HttpError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("GitHub API request failed")),
        }
    } else {
        let url = format!("{APP_REPO_API_URL}/releases?per_page=20");
        github_req(url)
            .send()
            .await
            .context("GitHub API request failed")?
            .json()
            .context("failed to parse releases JSON")?
    };

    // Find the best matching release for this channel. Use full semver
    // ordering (handles -rc/-beta/-alpha suffixes) so an rc.15 tag doesn't
    // get out-ranked by a stale stable v0.0.2.
    let release = releases
        .into_iter()
        .filter(|r| channel.accepts(&r.tag_name))
        .max_by(|a, b| {
            if is_newer_full(&a.tag_name, &b.tag_name) {
                std::cmp::Ordering::Greater
            } else if is_newer_full(&b.tag_name, &a.tag_name) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });

    let release = match release {
        Some(r) => r,
        None => return Ok(None),
    };

    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer_full(latest, CURRENT_VERSION) {
        return Ok(None);
    }

    // Release-asset naming schemes accepted, in order of preference:
    //   1. versioned   — `orca-0.0.4-x86_64-unknown-linux-gnu` (current pipeline, v0.0.4+)
    //   2. legacy      — `orca-x86_64-unknown-linux-gnu`       (pipeline ≤ v0.0.3)
    // Try versioned first so a re-issued release that includes both still
    // resolves to the canonical name.
    let versioned_name = format!("{APP_NAME}-{latest}-{BUILD_TARGET}");
    let legacy_name = format!("{APP_NAME}-{BUILD_TARGET}");

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == versioned_name)
        .or_else(|| release.assets.iter().find(|a| a.name == legacy_name))
        .with_context(|| {
            format!(
                "no asset '{versioned_name}' or '{legacy_name}' in release {}",
                release.tag_name
            )
        })?;
    let checksum_name = format!("{}.sha256", asset.name);
    let asset_url = asset.url.clone();

    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .map(|a| a.url.clone())
        .with_context(|| {
            format!(
                "no checksum asset '{checksum_name}' in release {} — refusing to advertise an unverifiable update",
                release.tag_name
            )
        })?;

    Ok(Some(UpdateInfo {
        version: latest.to_string(),
        asset_url,
        checksum_url,
    }))
}

/// Download the new binary, verify its checksum, and atomically replace the
/// current binary. Token must be the same one used for `check_for_update`.
pub async fn apply_update(info: &UpdateInfo, token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("no github token available for binary download");
    }
    let client = utils::http::Client::new();

    require_checksum_url(&info.version, &info.checksum_url)?;

    let cs_bytes = download_asset(&client, &info.checksum_url, token).await?;
    let cs_str = String::from_utf8_lossy(&cs_bytes);
    // Format: "<hash>  <filename>"
    let expected = cs_str
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .with_context(|| format!("checksum file empty at {}", info.checksum_url))?;

    println!("[orca] downloading v{}...", info.version);
    let binary = download_asset(&client, &info.asset_url, token).await?;

    verify_sha256(&binary, &expected)?;
    println!("[orca] checksum OK");

    // Write to a temp file beside the current binary, then atomic rename
    let current = current_binary_path()?;
    let tmp = current.with_extension("tmp");

    std::fs::write(&tmp, &binary).context("failed to write temp binary")?;

    // Set executable bit on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, &current).context("failed to replace binary")?;

    // macOS: ad-hoc sign so Gatekeeper accepts the new binary on next launch.
    // Without this the launchd daemon gets SIGKILLed on respawn (exit -9).
    #[cfg(target_os = "macos")]
    {
        _ = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&current)
            .status();
    }

    // Unraid: also mirror the new binary to /boot (USB), otherwise the update
    // is wiped on next reboot when the RAM rootfs resets. See
    // `install_unraid()` in commands/daemon.rs for the persistence contract.
    #[cfg(target_os = "linux")]
    if is_unraid() {
        let persist_bin = std::path::Path::new("/boot/config/plugins/orca/bin/orca");
        if let Some(parent) = persist_bin.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(&current, persist_bin) {
            Ok(_) => {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(persist_bin, std::fs::Permissions::from_mode(0o755));
                println!("[orca] mirrored to {} (unraid USB)", persist_bin.display());
            }
            Err(e) => {
                tracing::warn!(
                    "unraid USB mirror to {} failed: {e:#} — update will not survive reboot",
                    persist_bin.display()
                );
            }
        }
    }

    println!("[orca] updated to v{} — scheduling restart", info.version);
    schedule_self_restart();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn is_unraid() -> bool {
    std::fs::read_to_string("/etc/os-release")
        .map(|s| s.contains("ID=unraid-os") || s.contains("ID=\"unraid-os\""))
        .unwrap_or(false)
}

/// Detach a 2s delayed restart of whichever supervisor owns this daemon
/// (launchd on macOS, systemd-user / systemd-system on Linux). The delay
/// lets the in-flight update RPC return its response before SIGTERM lands;
/// the supervisor then respawns with the freshly-written binary.
///
/// Falls back to a plain SIGTERM-to-self for daemons not under a supervisor
/// (e.g. nohup'd dev runs) — they have to be restarted manually, but at
/// least we don't keep serving a deleted-inode old binary.
fn schedule_self_restart() {
    // `sh -c` is intentional here: we need `sleep N; if ... fi` executed as a
    // single detached background process. The only dynamic value is `my_pid`
    // which is a `u32` (no shell-special chars possible). All other content is
    // a compile-time static.
    let my_pid = std::process::id();
    #[cfg(target_os = "macos")]
    let cmd = format!(
        "sleep 2; if launchctl list 2>/dev/null | grep -q com.orca.daemon; then \
             launchctl kickstart -k gui/$(id -u)/com.orca.daemon; \
         else kill -TERM {my_pid}; fi"
    );
    #[cfg(target_os = "linux")]
    let cmd = format!(
        "sleep 2; \
         if command -v systemctl >/dev/null 2>&1 && systemctl --user is-active orca.service >/dev/null 2>&1; then \
             systemctl --user restart orca.service; \
         elif command -v systemctl >/dev/null 2>&1 && systemctl is-active orca.service >/dev/null 2>&1; then \
             systemctl restart orca.service; \
         else kill -TERM {my_pid}; fi"
    );
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let cmd = format!("sleep 2; kill -TERM {my_pid}");

    _ = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub async fn download_asset(
    client: &utils::http::Client,
    url: &str,
    token: &str,
) -> Result<Vec<u8>> {
    // Release binaries are ~30 MiB; the default 8 MiB http cap rejects them.
    const MAX_ASSET_BYTES: usize = 128 * 1024 * 1024;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", format!("{APP_NAME}/{CURRENT_VERSION}"))
        .max_body(MAX_ASSET_BYTES)
        .timeout(std::time::Duration::from_secs(300))
        .send_bytes()
        .await
        .context("download failed")?;
    Ok(resp.body)
}

pub fn current_binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("cannot determine current binary path")
}

// ── sha256 helpers ────────────────────────────────────────────────────────────

/// Verify `data` matches `expected` hex sha256. Returns `Err` on mismatch.
pub fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    let got = utils::hash::sha256_hex(data);
    if got != expected {
        bail!("checksum mismatch — expected {expected}, got {got}");
    }
    Ok(())
}

/// Guard: bail if `checksum_url` is empty (refuse unverifiable install).
pub fn require_checksum_url(version: &str, checksum_url: &str) -> Result<()> {
    if checksum_url.is_empty() {
        bail!("update refused: no checksum URL on release v{}", version);
    }
    Ok(())
}

/// Guard: bail if `sha256` is empty (refuse unverifiable dev install).
pub fn require_sha256_nonempty(sha256: &str) -> Result<()> {
    if sha256.is_empty() {
        bail!("dev-source returned empty sha256 — refusing unverifiable install");
    }
    Ok(())
}

// ── sha256 cache for `--check` ───────────────────────────────────────────────
//
// `orca update --check` is a cheap preview: it resolves the target version
// and pre-fetches the `.sha256` blob so a subsequent `orca update` (or an
// out-of-band download via install.sh) can verify against the cached hash
// without round-tripping to GitHub a second time.
//
// Cache shape:   $ORCA_HOME/cache/sha256/<version>.sha256
// TTL:           14 days (CHECK_CACHE_TTL_SECS) — large enough that nightly
//                CI smoke runs reuse a single hash; small enough that stale
//                entries from abandoned RC trains don't linger forever.
// Pruning:       lazy. Every `--check` walks the cache dir once and removes
//                anything past TTL. Cheap (≤ N files, single stat each).

const CHECK_CACHE_TTL_SECS: u64 = 14 * 24 * 3600;

fn check_cache_dir() -> Option<PathBuf> {
    Some(utils::fs::orca_home()?.join("cache").join("sha256"))
}

/// Drop any cached sha256 files older than `CHECK_CACHE_TTL_SECS`. Best-effort
/// — read/stat failures are skipped, never propagated.
pub fn prune_check_cache() {
    let Some(dir) = check_cache_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age.as_secs() > CHECK_CACHE_TTL_SECS {
            _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Path where a given version's sha256 is cached.
fn cached_sha256_path(version: &str) -> Option<PathBuf> {
    Some(check_cache_dir()?.join(format!("{version}.sha256")))
}

/// Write a checksum blob to the cache. Touches mtime so TTL is from "last
/// observed" rather than "first written" — a long-lived RC that keeps
/// re-validating stays warm.
pub fn write_cached_sha256(version: &str, body: &[u8]) -> Result<PathBuf> {
    let path = cached_sha256_path(version).context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_sha256_matches() {
        verify_sha256(
            b"hello",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        )
        .unwrap();
    }

    #[test]
    fn verify_sha256_mismatch_returns_err() {
        let err = verify_sha256(b"hello", "deadbeef").unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn require_checksum_url_ok() {
        require_checksum_url("0.0.4", "https://example.com/asset.sha256").unwrap();
    }

    #[test]
    fn require_checksum_url_empty_returns_err() {
        let err = require_checksum_url("0.0.4", "").unwrap_err();
        assert!(err.to_string().contains("no checksum URL"));
    }

    #[test]
    fn require_sha256_nonempty_ok() {
        require_sha256_nonempty("abc123").unwrap();
    }

    #[test]
    fn require_sha256_nonempty_empty_returns_err() {
        let err = require_sha256_nonempty("").unwrap_err();
        assert!(err.to_string().contains("empty sha256"));
    }
}
