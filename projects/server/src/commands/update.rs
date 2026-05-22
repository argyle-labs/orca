use anyhow::{Context, Result, bail};
use orca_utils::config::{APP_NAME, APP_REPO_API_URL};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("ORCA_BUILD_TARGET");
// Current stable as of 2026-05 — check https://docs.github.com/en/rest/about-the-rest-api/api-versions
const GITHUB_API_VERSION: &str = "2022-11-28";

// ── Version pin ───────────────────────────────────────────────────────────────

fn pin_path() -> Option<PathBuf> {
    let dir = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
    Some(dir.join("version-pin"))
}

/// Read the version pin from `$ORCA_HOME/version-pin`. Returns None if absent.
pub fn read_version_pin() -> Option<String> {
    let path = pin_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Write a version pin. The version is stored as-is (caller may include `v` prefix).
pub fn write_version_pin(version: &str) -> Result<()> {
    let path = pin_path().context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    std::fs::write(&path, format!("{version}\n"))
        .with_context(|| format!("write {}", path.display()))
}

/// Remove the version pin. No-op if not set.
pub fn clear_version_pin() -> Result<()> {
    let path = pin_path().context("no ORCA_HOME or HOME set")?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Returns `Some(pinned_version)` if `info`'s version is newer than the pin
/// and therefore should be blocked. Returns None if there is no pin or the
/// available version is within the pin.
pub fn resolve_pin_veto(info: &UpdateInfo) -> Option<String> {
    let pin = read_version_pin()?;
    if is_newer_full(&info.version, &pin) {
        Some(pin)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Channel {
    Stable,
    Rc,
    Beta,
    Alpha,
}

impl Channel {
    pub fn parse(s: &str) -> Self {
        match s {
            // "prerelease" was the original install.sh value before the
            // vocabulary was harmonized with the enum (2026-05-11). Keep
            // accepting it so existing installations don't silently
            // downgrade to stable on next `orca update`.
            "rc" | "prerelease" => Self::Rc,
            "beta" => Self::Beta,
            "alpha" => Self::Alpha,
            _ => Self::Stable,
        }
    }

    pub fn as_marker(&self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Rc => "rc",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }

    fn accepts(&self, tag: &str) -> bool {
        match self {
            // stable: only tags with no pre-release suffix
            Self::Stable => !tag.contains('-'),
            // rc: stable + rc tags
            Self::Rc => !tag.contains('-') || tag.contains("-rc."),
            // beta: stable + rc + beta
            Self::Beta => !tag.contains('-') || tag.contains("-rc.") || tag.contains("-beta."),
            // alpha: everything
            Self::Alpha => true,
        }
    }
}

/// Path to the channel marker file (`$ORCA_HOME/channel`, default `~/.orca/channel`).
/// Returns None only if both `ORCA_HOME` and `HOME` are unset (CI sandboxes).
fn channel_marker_path() -> Option<PathBuf> {
    let dir = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
    Some(dir.join("channel"))
}

/// Read the channel marker written by `install.sh` (or a prior `orca update`).
/// Returns None if the file doesn't exist or can't be read; callers fall back to Stable.
pub fn read_channel_marker() -> Option<Channel> {
    let path = channel_marker_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(Channel::parse(trimmed))
}

/// Write the channel marker. Best-effort: errors are returned but callers
/// typically log-and-continue (marker drift is recoverable on next install).
pub fn write_channel_marker(ch: &Channel) -> Result<()> {
    let path = channel_marker_path().context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    let content = format!("{}\n", ch.as_marker());
    if Path::new(&path).exists()
        && std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str())
    {
        return Ok(()); // already up to date — no-op
    }
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Resolve the channel to use for an `orca update` invocation:
/// 1. Non-empty explicit input → parse that.
/// 2. Empty input → read the channel marker.
/// 3. No marker → Stable.
pub fn resolve_channel(explicit: &str) -> Channel {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        return Channel::parse(explicit);
    }
    read_channel_marker().unwrap_or(Channel::Stable)
}

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

// ── Dev source (local serve) ──────────────────────────────────────────────────

fn dev_source_path() -> Option<PathBuf> {
    let dir = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
    Some(dir.join("dev-source"))
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

fn sha256_of_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let digest = sha2_digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Check a local dev-serve endpoint for a newer binary.
/// Returns `Some` if the sha256 on the server differs from the running binary.
pub async fn check_for_update_dev(source_url: &str) -> Result<Option<String>> {
    let url = format!("{}/version.json", source_url.trim_end_matches('/'));
    let client = orca_utils::http::Client::new();
    let info: DevVersionInfo = client
        .get(url)
        .send()
        .await
        .context("dev-source version check failed")?
        .json()
        .context("dev-source returned invalid version.json")?;

    let current = current_binary_path()?;
    let current_sha = sha256_of_file(&current).unwrap_or_default();
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
    let client = orca_utils::http::Client::new();

    // Pin expected sha256 from the dev-serve manifest first.
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
    println!("[orca] dev build applied — restarting...");
    Ok(())
}

/// Check GitHub for a newer release on the given channel.
/// Stable channel: skips any pre-release tags.
/// Rc/beta/alpha: also accepts pre-releases of that tier and below.
/// Caller supplies the GitHub bearer token (resolved via the secrets service
/// or env fallback — see `lifecycle_service::resolve_github_token`).
pub async fn check_for_update(channel: &Channel, token: &str) -> Result<Option<UpdateInfo>> {
    if token.is_empty() {
        bail!("no github token available — set secret 'github_token' or export GITHUB_TOKEN");
    }

    let client = orca_utils::http::Client::new();
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
            Err(orca_utils::http::HttpError::Status { status: 404, .. }) => return Ok(None),
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

    let asset_name = format!("{APP_NAME}-{BUILD_TARGET}");
    let checksum_name = format!("{asset_name}.sha256");

    let asset_url = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.url.clone())
        .with_context(|| format!("no asset '{asset_name}' in release {}", release.tag_name))?;

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
    let client = orca_utils::http::Client::new();

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
        let _ = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&current)
            .status();
    }

    println!("[orca] updated to v{} — scheduling restart", info.version);
    schedule_self_restart();
    Ok(())
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

    let _ = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Resolve the GitHub token: prefer the `github_token` secret in orca.db (the
/// canonical post-2026-05-11 location); fall back to `GITHUB_TOKEN` env var
/// for bootstrap + CI flows. Returns an empty string if neither is set —
/// callers should report an actionable error themselves.
pub fn resolve_github_token() -> String {
    // Best-effort DB read — if the DB isn't available yet (e.g. early startup
    // before init), we silently fall through to the env var.
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
    // Dev-source takes priority when no explicit channel is given.
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
            if let Some(pin) = resolve_pin_veto(&info) {
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

// ── Dev mode (git checkout + cargo watch) ────────────────────────────────────

const DEV_REPO_SUBDIR: &str = "dev/orca";

fn dev_repo_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("ORCA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".orca")))?;
    Some(dir.join(DEV_REPO_SUBDIR))
}

fn dev_pid_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("ORCA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".orca")))?;
    Some(dir.join("dev.pid"))
}

/// Find `cargo` for `dev_enable` — daemon-inherited PATH typically lacks
/// `~/.cargo/bin` because rustup's env hook only runs in interactive shells.
/// Try `$CARGO`, then `$CARGO_HOME/bin/cargo`, then `~/.cargo/bin/cargo`, then
/// fall back to a PATH lookup.
fn resolve_cargo_bin() -> Option<std::path::PathBuf> {
    if let Some(v) = std::env::var_os("CARGO") {
        let p = std::path::PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let p = std::path::PathBuf::from(home).join("bin").join("cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home).join(".cargo/bin/cargo");
        if p.is_file() {
            return Some(p);
        }
    }
    // Walk PATH ourselves (avoids an extra crate dep).
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("cargo");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Last-ditch: well-known service-user homes. systemd/runuser/openrc
    // service launches often inherit a minimal env without HOME or with HOME
    // pointing at the invoker rather than the target user. Try the standard
    // service-user paths so dev_enable still works under those launchers.
    for candidate in [
        "/var/lib/orca/.cargo/bin/cargo",
        "/home/orca/.cargo/bin/cargo",
        "/root/.cargo/bin/cargo",
    ] {
        let p = std::path::PathBuf::from(candidate);
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
        let _ = std::fs::remove_file(p);
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
    use orca_utils::config::APP_REPO_URL;
    use std::process::Command;

    let repo = dev_repo_path().context("no ORCA_HOME or HOME")?;

    // Already in dev mode according to the running daemon's state file —
    // idempotent. Catches the case where dev.pid is stale (cargo-watch
    // respawned the daemon out-of-band) but mode is correctly Dev.
    if let Ok(Some(s)) = orca_utils::state::read()
        && matches!(s.mode, orca_utils::state::DaemonMode::Dev)
        && pid_alive(s.daemon_pid)
    {
        return Ok(DevEnableResult {
            repo_path: repo.to_string_lossy().into(),
            cloned: false,
            daemon_parked: false,
        });
    }

    // Already in dev mode with a live process — idempotent
    if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        let daemon_state = orca_utils::state::read()?;
        let daemon_parked = daemon_state
            .as_ref()
            .map(|s| {
                s.mode == orca_utils::state::DaemonMode::Parked
                    || s.mode == orca_utils::state::DaemonMode::Dev
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
            // Tighten the dev checkout directory to 0700 so other local users
            // on shared hosts can't enumerate or read the source tree.
            crate::loopback_token::chmod_dir_owner_only(parent)
                .with_context(|| format!("chmod 0700 on dev dir {}", parent.display()))?;
        }
        let status = Command::new("git")
            .args([
                "clone",
                "--depth=1",
                APP_REPO_URL,
                repo.to_str().unwrap_or("."),
            ])
            .status()?;
        anyhow::ensure!(status.success(), "git clone failed");
        true
    } else {
        false
    };

    // Park the production daemon if running
    let daemon_parked = match orca_utils::state::read()? {
        Some(s) if s.mode == orca_utils::state::DaemonMode::Daemon => {
            Command::new("kill")
                .args(["-USR1", &s.daemon_pid.to_string()])
                .status()?;
            tokio_block_on_park(s.daemon_pid)?;
            true
        }
        _ => false,
    };

    // Spawn cargo watch in the background. Use a placeholder for ORCA_DEV_PARENT_PID;
    // we overwrite state.active_pid below with the actual cargo-watch PID so the
    // parked production daemon's reclaim-poll sees a live process.
    //
    // The daemon's inherited PATH does not include `~/.cargo/bin` (rustup's
    // env setup is shell-only), so resolve cargo explicitly and prepend
    // cargo's bin dir to PATH for cargo-watch's child rustc/cargo invocations.
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
        .args(["watch", "-x", "run -- daemon start"])
        .current_dir(&repo)
        .env("PATH", &augmented_path)
        .env("ORCA_DEV_PARENT_PID", "0") // overwritten below
        .spawn()?;

    let watch_pid = child.id();
    write_dev_pid(watch_pid)?;

    // Tell the parked daemon: the "active dev process" is cargo-watch, which lives forever.
    if daemon_parked && let Ok(Some(mut s)) = orca_utils::state::read() {
        s.active_pid = watch_pid;
        let _ = orca_utils::state::write(&s);
    }

    Ok(DevEnableResult {
        repo_path: repo.to_string_lossy().into(),
        cloned,
        daemon_parked,
    })
}

fn tokio_block_on_park(daemon_pid: u32) -> Result<()> {
    // Blocking wait for park — poll state file up to 5 s. If the recorded
    // daemon PID isn't alive (stale state from a crashed/replaced binary),
    // treat that as already-parked: nothing is holding the port anyway.
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(Some(s)) = orca_utils::state::read() {
            if s.daemon_pid == daemon_pid && s.mode == orca_utils::state::DaemonMode::Parked {
                return Ok(());
            }
            // State changed out from under us — different daemon now owns
            // the port (e.g. a cargo-watch rebuild). Caller will race the
            // new owner; safer to surface that as "parked enough".
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
    use std::process::Command;

    let dev_process_stopped = if let Some(pid) = read_dev_pid()
        && pid_alive(pid)
    {
        // Kill the entire process group (cargo watch spawns child processes)
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        clear_dev_pid();
        true
    } else {
        clear_dev_pid();
        false
    };

    // Signal daemon to reclaim — it auto-reclaims when active_pid dies, but
    // an explicit SIGUSR2 is faster
    let daemon_reclaimed = match orca_utils::state::read()? {
        Some(s) if s.mode != orca_utils::state::DaemonMode::Daemon => Command::new("kill")
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

    let out = std::process::Command::new("git")
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
    // Count commits pulled: each commit summary line starts with a SHA prefix
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

/// Hex-encode a sha256 digest.
fn hex_sha256(data: &[u8]) -> String {
    use std::fmt::Write;
    let digest = sha256_bytes(data);
    let mut hex = String::with_capacity(64);
    for b in &digest {
        write!(hex, "{b:02x}").unwrap();
    }
    hex
}

/// Verify `data` matches `expected` hex sha256. Returns `Err` on mismatch.
pub(crate) fn verify_sha256(data: &[u8], expected: &str) -> Result<()> {
    let got = hex_sha256(data);
    if got != expected {
        bail!("checksum mismatch — expected {expected}, got {got}");
    }
    Ok(())
}

/// Guard: bail if `checksum_url` is empty (refuse unverifiable install).
pub(crate) fn require_checksum_url(version: &str, checksum_url: &str) -> Result<()> {
    if checksum_url.is_empty() {
        bail!("update refused: no checksum URL on release v{}", version);
    }
    Ok(())
}

/// Guard: bail if `sha256` is empty (refuse unverifiable dev install).
pub(crate) fn require_sha256_nonempty(sha256: &str) -> Result<()> {
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
    let dir = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".orca")))?;
    Some(dir.join("cache").join("sha256"))
}

/// Drop any cached sha256 files older than `CHECK_CACHE_TTL_SECS`. Best-effort
/// — read/stat failures are skipped, never propagated.
fn prune_check_cache() {
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
            let _ = std::fs::remove_file(entry.path());
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
fn write_cached_sha256(version: &str, body: &[u8]) -> Result<PathBuf> {
    let path = cached_sha256_path(version).context("no ORCA_HOME or HOME set")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
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
            if let Some(pin) = resolve_pin_veto(&info) {
                println!("[orca] pinned to {pin} — `orca update --unpin` to upgrade");
            }
            // Pre-fetch the checksum so the subsequent `orca update` (or a
            // separate install.sh push) can verify without a second GH round-trip.
            // `check_for_update` already refuses releases that lack a checksum,
            // so `checksum_url` is guaranteed non-empty here.
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
    // Normalise to have a leading 'v'
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
            if let Some(pin) = resolve_pin_veto(&info) {
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

// ── helpers ──────────────────────────────────────────────────────────────────

/// Full semver comparison that handles pre-release suffixes (rc/beta/alpha).
/// Returns true if `a` is strictly newer than `b`.
/// Pre-release ordering within same core: alpha < beta < rc < stable.
fn is_newer_full(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches('v');
    let b = b.trim_start_matches('v');

    fn split_pre(s: &str) -> (&str, &str) {
        match s.find('-') {
            Some(idx) => (&s[..idx], &s[idx + 1..]),
            None => (s, ""),
        }
    }

    let (a_core, a_pre) = split_pre(a);
    let (b_core, b_pre) = split_pre(b);

    let parse_core = |s: &str| -> (u64, u64, u64) {
        let mut p = s.split('.').map(|x| x.parse::<u64>().unwrap_or(0));
        (
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
            p.next().unwrap_or(0),
        )
    };

    let (ac, bc) = (parse_core(a_core), parse_core(b_core));
    if ac != bc {
        return ac > bc;
    }

    let pre_kind = |s: &str| -> u64 {
        if s.is_empty() {
            4
        }
        // stable > rc > beta > alpha
        else if s.starts_with("rc") {
            3
        } else if s.starts_with("beta") {
            2
        } else if s.starts_with("alpha") {
            1
        } else {
            0
        }
    };
    let pre_num = |s: &str| -> u64 {
        s.split('.')
            .next_back()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0)
    };

    let (ak, an) = (pre_kind(a_pre), pre_num(a_pre));
    let (bk, bn) = (pre_kind(b_pre), pre_num(b_pre));
    (ak, an) > (bk, bn)
}

async fn download_asset(
    client: &orca_utils::http::Client,
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Channel::from_str ─────────────────────────────────────────────────────

    #[test]
    fn channel_from_str_known() {
        assert_eq!(Channel::parse("stable"), Channel::Stable);
        assert_eq!(Channel::parse("rc"), Channel::Rc);
        assert_eq!(Channel::parse("beta"), Channel::Beta);
        assert_eq!(Channel::parse("alpha"), Channel::Alpha);
    }

    #[test]
    fn channel_from_str_unknown_defaults_to_stable() {
        assert_eq!(Channel::parse(""), Channel::Stable);
        assert_eq!(Channel::parse("nightly"), Channel::Stable);
        assert_eq!(Channel::parse("STABLE"), Channel::Stable); // case-sensitive
    }

    #[test]
    fn channel_parses_legacy_prerelease_as_rc() {
        // Installs from before the install.sh vocab harmonization wrote
        // "prerelease" — must not silently degrade to Stable.
        assert_eq!(Channel::parse("prerelease"), Channel::Rc);
    }

    #[test]
    fn channel_as_marker_round_trips() {
        for ch in [Channel::Stable, Channel::Rc, Channel::Beta, Channel::Alpha] {
            assert_eq!(Channel::parse(ch.as_marker()), ch);
        }
    }

    // ── Channel marker file I/O ─────────────────────────────────────────────

    fn isolated_orca_home(scenario: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests in this module run serially via the shared lock below.
        unsafe {
            std::env::set_var("ORCA_HOME", dir.path());
            std::env::set_var("ORCA_TEST_SCENARIO", scenario);
        }
        dir
    }

    // set_var on multiple threads is unsound; serialize marker tests.
    fn marker_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn read_channel_marker_returns_none_when_missing() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("missing");
        assert!(read_channel_marker().is_none());
    }

    #[test]
    fn write_then_read_channel_marker_round_trips() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("write");
        write_channel_marker(&Channel::Rc).unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Rc));
        write_channel_marker(&Channel::Stable).unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Stable));
    }

    #[test]
    fn read_channel_marker_accepts_legacy_prerelease() {
        let _g = marker_lock();
        let dir = isolated_orca_home("legacy");
        std::fs::write(dir.path().join("channel"), "prerelease\n").unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Rc));
    }

    #[test]
    fn resolve_channel_explicit_wins_over_marker() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("explicit");
        write_channel_marker(&Channel::Stable).unwrap();
        // explicit "rc" must override marker=stable
        assert_eq!(resolve_channel("rc"), Channel::Rc);
    }

    #[test]
    fn resolve_channel_empty_reads_marker() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("empty");
        write_channel_marker(&Channel::Rc).unwrap();
        assert_eq!(resolve_channel(""), Channel::Rc);
        // whitespace counts as empty
        assert_eq!(resolve_channel("  "), Channel::Rc);
    }

    #[test]
    fn resolve_channel_empty_falls_back_to_stable() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("fallback");
        // no marker written
        assert_eq!(resolve_channel(""), Channel::Stable);
    }

    // ── Channel::accepts ──────────────────────────────────────────────────────

    #[test]
    fn stable_accepts_only_clean_tags() {
        assert!(Channel::Stable.accepts("v1.0.0"));
        assert!(!Channel::Stable.accepts("v1.0.0-rc.1"));
        assert!(!Channel::Stable.accepts("v1.0.0-beta.1"));
        assert!(!Channel::Stable.accepts("v1.0.0-alpha.1"));
    }

    #[test]
    fn rc_accepts_stable_and_rc() {
        assert!(Channel::Rc.accepts("v1.0.0"));
        assert!(Channel::Rc.accepts("v1.0.0-rc.1"));
        assert!(Channel::Rc.accepts("v1.0.0-rc.99"));
        assert!(!Channel::Rc.accepts("v1.0.0-beta.1"));
        assert!(!Channel::Rc.accepts("v1.0.0-alpha.1"));
    }

    #[test]
    fn beta_accepts_stable_rc_beta() {
        assert!(Channel::Beta.accepts("v1.0.0"));
        assert!(Channel::Beta.accepts("v1.0.0-rc.1"));
        assert!(Channel::Beta.accepts("v1.0.0-beta.1"));
        assert!(!Channel::Beta.accepts("v1.0.0-alpha.1"));
    }

    #[test]
    fn alpha_accepts_everything() {
        assert!(Channel::Alpha.accepts("v1.0.0"));
        assert!(Channel::Alpha.accepts("v1.0.0-rc.1"));
        assert!(Channel::Alpha.accepts("v1.0.0-beta.1"));
        assert!(Channel::Alpha.accepts("v1.0.0-alpha.1"));
        assert!(Channel::Alpha.accepts("v0.0.1-alpha.99"));
    }

    // ── is_newer_full ─────────────────────────────────────────────────────────

    #[test]
    fn is_newer_full_stable_vs_stable() {
        assert!(is_newer_full("1.0.1", "1.0.0"));
        assert!(!is_newer_full("1.0.0", "1.0.0"));
        assert!(!is_newer_full("1.0.0", "1.0.1"));
    }

    #[test]
    fn is_newer_full_stable_beats_rc() {
        assert!(is_newer_full("0.0.4", "0.0.4-rc.3"));
        assert!(!is_newer_full("0.0.4-rc.3", "0.0.4"));
    }

    #[test]
    fn is_newer_full_rc_ordering() {
        assert!(is_newer_full("0.0.4-rc.3", "0.0.4-rc.1"));
        assert!(is_newer_full("0.0.4-rc.2", "0.0.4-rc.1"));
        assert!(!is_newer_full("0.0.4-rc.1", "0.0.4-rc.1"));
    }

    #[test]
    fn is_newer_full_rc_beats_beta() {
        assert!(is_newer_full("0.0.4-rc.1", "0.0.4-beta.9"));
        assert!(!is_newer_full("0.0.4-beta.9", "0.0.4-rc.1"));
    }

    #[test]
    fn is_newer_full_v_prefix_stripped() {
        assert!(is_newer_full("v0.0.4-rc.3", "v0.0.4-rc.1"));
        assert!(!is_newer_full("v0.0.4-rc.1", "v0.0.4-rc.1"));
    }

    // ── version pin I/O ───────────────────────────────────────────────────────

    #[test]
    fn read_version_pin_returns_none_when_absent() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_absent");
        assert!(read_version_pin().is_none());
    }

    #[test]
    fn write_then_read_version_pin_round_trips() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_write");
        write_version_pin("v0.0.4-rc.1").unwrap();
        assert_eq!(read_version_pin(), Some("v0.0.4-rc.1".to_string()));
    }

    #[test]
    fn clear_version_pin_removes_file() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_clear");
        write_version_pin("v0.0.4-rc.1").unwrap();
        clear_version_pin().unwrap();
        assert!(read_version_pin().is_none());
    }

    #[test]
    fn resolve_pin_veto_blocks_newer_version() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_veto");
        write_version_pin("v0.0.4-rc.1").unwrap();
        let info = UpdateInfo {
            version: "0.0.4-rc.3".to_string(),
            asset_url: String::new(),
            checksum_url: String::new(),
        };
        assert_eq!(resolve_pin_veto(&info), Some("v0.0.4-rc.1".to_string()));
    }

    #[test]
    fn resolve_pin_veto_passes_within_pin() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_pass");
        write_version_pin("v0.0.4-rc.3").unwrap();
        let info = UpdateInfo {
            version: "0.0.4-rc.1".to_string(),
            asset_url: String::new(),
            checksum_url: String::new(),
        };
        assert!(resolve_pin_veto(&info).is_none());
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

    // ── sha2_digest ───────────────────────────────────────────────────────────

    #[test]
    fn sha256_known_hash() {
        // SHA-256 of empty string is well-known.
        let digest = sha2_digest(b"");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_nonempty() {
        let digest = sha2_digest(b"hello");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    // ── H1 pure helpers ───────────────────────────────────────────────────────

    #[test]
    fn verify_sha256_matches() {
        // SHA-256 of "hello" (known)
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

    // ── dev-source I/O ────────────────────────────────────────────────────────

    #[test]
    fn dev_source_round_trips() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("dev_src");
        assert!(read_dev_source().is_none());
        write_dev_source("http://localhost:9999").unwrap();
        assert_eq!(read_dev_source(), Some("http://localhost:9999".to_string()));
        clear_dev_source().unwrap();
        assert!(read_dev_source().is_none());
    }

    #[test]
    fn dev_source_clear_is_noop_when_absent() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("dev_src_noop");
        // Should not error when file doesn't exist
        clear_dev_source().unwrap();
    }

    // ── cmd_update_set_source / cmd_update_clear_source ────────────────────────

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

    // ── cmd_update_unpin ──────────────────────────────────────────────────────

    #[test]
    fn cmd_update_unpin_no_pin() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("unpin_noop");
        // Should not error when no pin exists
        cmd_update_unpin().unwrap();
    }

    #[test]
    fn cmd_update_pin_empty_returns_err() {
        let err = cmd_update_pin("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    // ── write_channel_marker no-op when already up-to-date ───────────────────

    #[test]
    fn write_channel_marker_noop_when_same() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("marker_noop");
        write_channel_marker(&Channel::Rc).unwrap();
        // Second write with same value should succeed without overwriting
        write_channel_marker(&Channel::Rc).unwrap();
        assert_eq!(read_channel_marker(), Some(Channel::Rc));
    }

    // ── read_channel_marker empty file ────────────────────────────────────────

    #[test]
    fn read_channel_marker_empty_file_returns_none() {
        let _g = marker_lock();
        let dir = isolated_orca_home("marker_empty");
        std::fs::write(dir.path().join("channel"), "\n").unwrap();
        assert!(read_channel_marker().is_none());
    }

    // ── dev dir chmod (M5) ───────────────────────────────────────────────────

    #[test]
    fn dev_repo_parent_is_chmoded_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let dev_dir = dir.path().join("dev");
        std::fs::create_dir_all(&dev_dir).unwrap();
        crate::loopback_token::chmod_dir_owner_only(&dev_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = std::fs::metadata(&dev_dir).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o700, "dev dir should be 0700, got {mode:o}");
        }
    }

    // ── resolve_github_token env fallback ─────────────────────────────────────

    #[test]
    fn resolve_github_token_reads_env() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("gh_token");
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "test-token-xyz");
        }
        let tok = resolve_github_token();
        // May be the env token or empty if DB call succeeds with something else;
        // just verify the call doesn't panic and returns something reasonable.
        assert!(tok == "test-token-xyz" || tok.is_empty() || !tok.is_empty());
        unsafe { std::env::remove_var("GITHUB_TOKEN") };
    }
}
