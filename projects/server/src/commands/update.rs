use anyhow::{Context, Result, bail};
use orca_utils::config::{APP_NAME, APP_REPO_API_URL};
use serde::Deserialize;
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

    // Find the best matching release for this channel
    let release = releases
        .into_iter()
        .filter(|r| channel.accepts(&r.tag_name))
        .max_by(|a, b| {
            let va = a.tag_name.trim_start_matches('v');
            let vb = b.tag_name.trim_start_matches('v');
            semver_cmp(va, vb)
        });

    let release = match release {
        Some(r) => r,
        None => return Ok(None),
    };

    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer(latest, CURRENT_VERSION) {
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
        .unwrap_or_default();

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

    // Download the checksum file first
    let expected_hash = if !info.checksum_url.is_empty() {
        let cs_bytes = download_asset(&client, &info.checksum_url, token).await?;
        let cs_str = String::from_utf8_lossy(&cs_bytes);
        // Format: "<hash>  <filename>"
        cs_str.split_whitespace().next().map(|s| s.to_string())
    } else {
        None
    };

    // Download the binary
    println!("[orca] downloading v{}...", info.version);
    let binary = download_asset(&client, &info.asset_url, token).await?;

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
        println!("[orca] checksum OK");
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
    println!("[orca] updated to v{} — restart to activate", info.version);

    Ok(())
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
pub async fn cmd_update(channel_arg: &str) -> Result<()> {
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

fn semver_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(a).cmp(&parse(b))
}

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

async fn download_asset(
    client: &orca_utils::http::Client,
    url: &str,
    token: &str,
) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", format!("{APP_NAME}/{CURRENT_VERSION}"))
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

    // ── semver_cmp ────────────────────────────────────────────────────────────

    #[test]
    fn semver_cmp_ordering() {
        use std::cmp::Ordering::*;
        assert_eq!(semver_cmp("1.0.1", "1.0.0"), Greater);
        assert_eq!(semver_cmp("1.1.0", "1.0.9"), Greater);
        assert_eq!(semver_cmp("2.0.0", "1.9.9"), Greater);
        assert_eq!(semver_cmp("1.0.0", "1.0.0"), Equal);
        assert_eq!(semver_cmp("1.0.0", "1.0.1"), Less);
        assert_eq!(semver_cmp("0.0.0", "0.0.0"), Equal);
    }

    #[test]
    fn semver_cmp_missing_parts_default_zero() {
        use std::cmp::Ordering::*;
        // "1.0" treated as "1.0.0"
        assert_eq!(semver_cmp("1.0", "1.0.0"), Equal);
        assert_eq!(semver_cmp("1", "1.0.0"), Equal);
    }

    // ── is_newer ──────────────────────────────────────────────────────────────

    #[test]
    fn is_newer_returns_true_when_candidate_greater() {
        assert!(is_newer("1.0.1", "1.0.0"));
        assert!(is_newer("2.0.0", "1.9.9"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
    }

    #[test]
    fn is_newer_strips_v_prefix() {
        // The function doesn't strip 'v' itself — callers already strip it via
        // trim_start_matches('v'). Verify it handles plain version strings.
        assert!(is_newer("1.2.0", "1.1.9"));
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
}
