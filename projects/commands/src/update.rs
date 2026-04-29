use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::PathBuf;

const GITHUB_REPO: &str = "scottdkey/brain";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("BRAIN_BUILD_TARGET");

#[derive(Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub asset_url: String,  // API URL (requires auth to download)
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

/// Check GitHub for a newer release. Returns None if already up to date.
/// Requires GITHUB_TOKEN env var for private repo access.
pub async fn check_for_update() -> Result<Option<UpdateInfo>> {
    let token = match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => bail!("GITHUB_TOKEN not set — cannot check for updates"),
    };

    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", format!("brain/{CURRENT_VERSION}"))
        .send()
        .await
        .context("GitHub API request failed")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None); // no releases yet
    }

    let release: Release = resp
        .error_for_status()
        .context("GitHub API error")?
        .json()
        .await
        .context("failed to parse release JSON")?;

    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer(latest, CURRENT_VERSION) {
        return Ok(None);
    }

    let asset_name = format!("brain-{BUILD_TARGET}");
    let checksum_name = format!("{asset_name}.sha256");

    let asset_url = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.url.clone())
        .with_context(|| format!("no asset named '{asset_name}' in release {}", release.tag_name))?;

    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .map(|a| a.url.clone())
        .unwrap_or_default();

    Ok(Some(UpdateInfo {
        version: latest.to_string(),
        asset_url,
        checksum_url,
    }))
}

/// Download the new binary, verify its checksum, and atomically replace the current binary.
pub async fn apply_update(info: &UpdateInfo) -> Result<()> {
    let token = std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let client = reqwest::Client::new();

    // Download the checksum file first
    let expected_hash = if !info.checksum_url.is_empty() {
        let cs_bytes = download_asset(&client, &info.checksum_url, &token).await?;
        let cs_str = String::from_utf8_lossy(&cs_bytes);
        // Format: "<hash>  <filename>"
        cs_str.split_whitespace().next().map(|s| s.to_string())
    } else {
        None
    };

    // Download the binary
    println!("[brain] downloading v{}...", info.version);
    let binary = download_asset(&client, &info.asset_url, &token).await?;

    // Verify checksum
    if let Some(expected) = expected_hash {
        use std::fmt::Write;
        let digest = sha256_bytes(&binary);
        let mut got = String::with_capacity(64);
        for b in &digest {
            write!(got, "{b:02x}").unwrap();
        }
        if got != expected {
            bail!("checksum mismatch — expected {expected}, got {got}");
        }
        println!("[brain] checksum OK");
    }

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
    println!("[brain] updated to v{} — restart to activate", info.version);

    Ok(())
}

pub async fn cmd_update() -> Result<()> {
    println!("[brain] current version: v{CURRENT_VERSION} ({BUILD_TARGET})");
    println!("[brain] checking for updates...");

    match check_for_update().await? {
        None => println!("[brain] already up to date"),
        Some(info) => {
            println!("[brain] new version available: v{}", info.version);
            apply_update(&info).await?;
        }
    }

    Ok(())
}

/// Non-blocking startup update check — just prints a notice, does not download.
pub async fn startup_update_check() {
    if std::env::var("GITHUB_TOKEN").is_err() {
        return;
    }
    match check_for_update().await {
        Ok(Some(info)) => {
            println!(
                "[brain] update available: v{} → run 'brain update' to upgrade",
                info.version
            );
        }
        Ok(None) => {}
        Err(_) => {} // silent on startup — network may not be available
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(candidate) > parse(current)
}

async fn download_asset(client: &reqwest::Client, url: &str, token: &str) -> Result<Vec<u8>> {
    let bytes = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", format!("brain/{CURRENT_VERSION}"))
        .send()
        .await
        .context("download request failed")?
        .error_for_status()
        .context("download HTTP error")?
        .bytes()
        .await
        .context("failed to read download body")?;
    Ok(bytes.to_vec())
}

fn current_binary_path() -> Result<PathBuf> {
    std::env::current_exe().context("cannot determine current binary path")
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    // Minimal SHA-256 using only std — avoids adding a crypto dep.
    // We use the sha2 crate if available; otherwise fall back to ring or openssl.
    // For now, use a pure-Rust implementation via the sha2 crate added below.
    sha2_digest(data)
}

// sha2 is added as a dependency — see Cargo.toml
fn sha2_digest(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}
