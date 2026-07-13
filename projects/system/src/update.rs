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
use contract::config::{APP_NAME, APP_REPO_API_URL};
use serde::Deserialize;
use std::path::PathBuf;

use crate::update_state::{Channel, is_newer_full};

/// Resolve the GitHub token: prefer the `github_token` secret in orca.db
/// (the canonical post-2026-05-11 location); fall back to `GITHUB_TOKEN` env
/// var for bootstrap + CI flows. Returns an empty string if neither is set —
/// callers should report an actionable error themselves.
///
/// Reads via [`db::open_canonical`], NOT `open_default()`: the secret lives only
/// in the canonical encrypted db, and a leaked `THREAD_DB_PATH` on a pooled
/// tokio worker (from a prior HTTP request) would otherwise point the read at an
/// unencrypted, secret-less db — the cause of the spurious "this peer has no
/// github_token" from the `system.serve_release` delegate-on-miss flow.
pub fn resolve_github_token() -> String {
    if let Ok(conn) = db::open_canonical()
        && let Ok(Some(_)) = db::secrets::get(&conn, "github_token")
        && let Ok(Some(v)) = db::secrets::read_inline_value(&conn, "github_token")
        && !v.is_empty()
    {
        return v;
    }
    std::env::var("GITHUB_TOKEN").unwrap_or_default()
}

const CURRENT_VERSION: &str = env!("ORCA_VERSION");
const BUILD_TARGET: &str = env!("ORCA_BUILD_TARGET");

/// Rust target triple this binary was compiled for. Exposed for the
/// delegate-on-miss flow: a peer that lacks `github_token` asks a paired
/// peer to fetch the release asset matching this target.
pub fn build_target() -> &'static str {
    BUILD_TARGET
}
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
/// Rc: also accepts `-rc.N` pre-releases.
/// Dev: returns None — dev channel updates via git, not GitHub releases.
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
    // For pre-release channels we need BOTH endpoints unioned:
    //   * /releases (paginated, includes rc tags) — provides newer rc candidates.
    //   * /releases/latest (always-current stable) — guarantees we never miss
    //     the newest stable even if 100 rc tags have shipped between stables.
    //     Without this, an Rc-channel host that's gone many rcs past the last
    //     stable would silently miss a newer stable upgrade — stable falls
    //     outside the per_page window. `Channel::Rc::accepts` allows stable,
    //     so the candidate just needs to be IN the response set.
    let releases: Vec<Release> = if *channel == Channel::Stable {
        let url = format!("{APP_REPO_API_URL}/releases/latest");
        match github_req(url).send().await {
            Ok(resp) => vec![resp.json().context("failed to parse release JSON")?],
            Err(utils::http::HttpError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("GitHub API request failed")),
        }
    } else {
        let list_url = format!("{APP_REPO_API_URL}/releases?per_page=100");
        let mut all: Vec<Release> = github_req(list_url)
            .send()
            .await
            .context("GitHub API request failed")?
            .json()
            .context("failed to parse releases JSON")?;

        // Always also fetch /releases/latest so a stale stable far past the
        // pagination window is still considered. 404 = repo has never had a
        // stable release — fine, the paginated list is sufficient.
        let latest_url = format!("{APP_REPO_API_URL}/releases/latest");
        match github_req(latest_url).send().await {
            Ok(resp) => {
                let stable: Release = resp.json().context("failed to parse latest release JSON")?;
                if !all.iter().any(|r| r.tag_name == stable.tag_name) {
                    all.push(stable);
                }
            }
            Err(utils::http::HttpError::Status { status: 404, .. }) => {}
            Err(e) => {
                return Err(anyhow::Error::from(e).context("GitHub latest-release request failed"));
            }
        }
        all
    };

    // Find the best matching release for this channel. Use full semver
    // ordering (handles -rc/-beta/-alpha suffixes) so an rc.15 tag doesn't
    // get out-ranked by a stale stable v0.0.2.
    let release = pick_best_release(releases, channel);

    let release = match release {
        Some(r) => r,
        None => return Ok(None),
    };

    let latest = release.tag_name.trim_start_matches('v');
    if !is_newer_full(latest, CURRENT_VERSION) {
        return Ok(None);
    }

    let (asset_name, asset_url) = select_asset(&release.assets, latest, BUILD_TARGET)
        .with_context(|| {
            format!(
                "no asset '{}' or '{}' in release {}",
                versioned_asset_name(latest, BUILD_TARGET),
                legacy_asset_name(BUILD_TARGET),
                release.tag_name
            )
        })?;
    let checksum_name = format!("{asset_name}.sha256");

    let checksum_url = select_checksum_url(&release.assets, &checksum_name)
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

/// Single entry in the version-picker list. Tag is the GitHub release tag
/// (with or without `v` prefix as returned by GitHub); `is_current` is true
/// when the tag matches the running binary's `CURRENT_VERSION`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct VersionEntry {
    pub tag: String,
    pub prerelease: bool,
    pub published_at: Option<String>,
    pub is_current: bool,
}

#[derive(Deserialize)]
struct ReleaseMeta {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
}

/// Return all releases visible on `channel`, newest first. Empty for
/// [`Channel::Dev`] (dev tracks local git HEAD, not GitHub releases).
pub async fn list_versions(channel: &Channel, token: &str) -> Result<Vec<VersionEntry>> {
    if matches!(channel, Channel::Dev) {
        return Ok(Vec::new());
    }
    if token.is_empty() {
        bail!("no github token available — set secret 'github_token' or export GITHUB_TOKEN");
    }

    let client = utils::http::Client::new();
    let user_agent = format!("{APP_NAME}/{CURRENT_VERSION}");
    let url = format!("{APP_REPO_API_URL}/releases?per_page=100");
    let releases: Vec<ReleaseMeta> = client
        .get(url)
        .bearer(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", &user_agent)
        .send()
        .await
        .context("GitHub API request failed")?
        .json()
        .context("failed to parse releases JSON")?;

    let mut entries: Vec<VersionEntry> = releases
        .into_iter()
        .filter(|r| channel.accepts(&r.tag_name))
        .map(|r| {
            let stripped = r.tag_name.trim_start_matches('v');
            VersionEntry {
                is_current: stripped == CURRENT_VERSION,
                tag: r.tag_name,
                prerelease: r.prerelease,
                published_at: r.published_at,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        if is_newer_full(&a.tag, &b.tag) {
            std::cmp::Ordering::Less // newer first
        } else if is_newer_full(&b.tag, &a.tag) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    Ok(entries)
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

    apply_binary(&binary, &info.version)
}

/// Swap the running binary with `bytes` and schedule a supervisor restart.
///
/// Caller is responsible for any pre-swap integrity check on `bytes`
/// (e.g. sha256 against a release checksum). This function handles only
/// what's downstream of the verified bytes: atomic tmp-write + rename,
/// post-swap on-disk re-verification with rollback, macOS codesigning,
/// Unraid appdata mirror with the self-copy guard, the pending_restart
/// marker, and the detached supervisor restart.
///
/// Extracted from `apply_update` (slice S1 of delegate-on-miss) so the
/// delegate-fetched-from-peer path can share the swap codepath without
/// going through GitHub. See [[project-github-token-auto-provision]].
pub fn apply_binary(bytes: &[u8], version: &str) -> Result<()> {
    // Write to a temp file beside the current binary, then atomic rename.
    // Stash the current binary first so we can roll back on post-swap
    // verification failure — silent zero-byte writes (e.g. FUSE shfs
    // truncating on /mnt/user/appdata) used to leave the host with no
    // working binary and a falsely-successful `applied` response.
    let current = current_binary_path()?;
    let tmp = current.with_extension("tmp");
    let backup = current.with_extension("prev");
    let backed_up = std::fs::copy(&current, &backup).is_ok();

    std::fs::write(&tmp, bytes).context("failed to write temp binary")?;

    // Set executable bit on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
    }

    std::fs::rename(&tmp, &current).context("failed to replace binary")?;

    // Verify what actually hit disk — not just what we held in memory. The
    // pre-rename sha check covers download integrity; this covers
    // filesystem-level corruption (truncation, partial writes on FUSE).
    if let Err(e) = verify_on_disk(&current, bytes, version) {
        if backed_up {
            // The swapped-in binary is corrupt; restore the backup. If that
            // ALSO fails, the live binary is left broken — say so plainly
            // rather than claiming a rollback that did not happen.
            if let Err(rollback) = std::fs::rename(&backup, &current) {
                return Err(e.context(format!(
                    "post-swap binary verification failed AND rollback failed \
                     ({rollback}); the binary at {} is corrupt — restore manually \
                     from {}",
                    current.display(),
                    backup.display()
                )));
            }
        }
        return Err(e.context("post-swap binary verification failed; rolled back"));
    }
    if backed_up {
        _ = std::fs::remove_file(&backup);
    }

    // macOS: ad-hoc sign so Gatekeeper accepts the new binary on next launch.
    // Without this the launchd daemon gets SIGKILLed on respawn (exit -9).
    #[cfg(target_os = "macos")]
    {
        let codesign_status = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-"])
            .arg(&current)
            .status()
            .context("invoking codesign")?;
        if !codesign_status.success() {
            anyhow::bail!("codesign failed with status {codesign_status}");
        }
    }

    // Unraid: mirror the new binary to appdata so it survives the RAM-rootfs
    // wipe on reboot. /mnt/user/appdata/orca/bin/ is a real ext4/xfs path owned
    // by the orca user — no sudo, no staging, no vfat permission dance.
    // `rc.orca` restores from this path on boot. See
    // [[project-unraid-persistence-via-appdata]] for the contract.
    #[cfg(target_os = "linux")]
    if is_unraid() {
        let persist_dir = std::path::Path::new("/mnt/user/appdata/orca/bin");
        let persist_bin = persist_dir.join("orca");
        // When the live rc.orca already runs the daemon directly out of
        // appdata, `current_binary_path()` IS `persist_bin`. Copying a file
        // onto itself with `std::fs::copy` opens dst with O_TRUNC before
        // reading src → silently truncates the binary to 0 bytes. This
        // exact path bricked alpha + echo twice (2026-06-02, 2026-06-03).
        let same_path = std::fs::canonicalize(&current).ok()
            == std::fs::canonicalize(&persist_bin).ok()
            && std::fs::canonicalize(&current).is_ok();
        if same_path {
            println!("[orca] running from appdata already — skipping persist mirror");
        } else {
            std::fs::create_dir_all(persist_dir)
                .with_context(|| format!("create unraid appdata dir {}", persist_dir.display()))?;
            std::fs::copy(&current, &persist_bin).map_err(|e| {
            anyhow::anyhow!(
                "mirror new binary to {} (unraid appdata persistence): {} (kind={:?}, errno={:?})",
                persist_bin.display(),
                e,
                e.kind(),
                e.raw_os_error(),
            )
        })?;
            // FUSE shfs on /mnt/user/appdata has been observed to leave a 0-byte
            // file behind while reporting success. Verify the mirror matches the
            // bytes we just installed; bail loudly if not so the host doesn't
            // come back from reboot to a broken binary.
            verify_on_disk(&persist_bin, bytes, version)
                .context("unraid appdata mirror verification failed")?;
            println!(
                "[orca] mirrored to {} (unraid appdata)",
                persist_bin.display()
            );
        }
    }

    println!("[orca] updated to v{version} — scheduling restart");
    write_pending_restart_marker(version);
    let method = schedule_self_restart();
    println!("[orca] restart method: {method}");
    Ok(())
}

/// Write a marker indicating an apply just completed and we expect the
/// daemon to come back on `target`. The post-restart daemon checks this on
/// startup; remote clients can read it via system.detail to verify the
/// swap actually took effect (apply returning success only means the bytes
/// hit disk — the supervisor restart is the part that's been silently
/// failing on hosts where the daemon runs as a non-root user without
/// polkit auth to `systemctl restart`).
pub(crate) fn write_pending_restart_marker(target: &str) {
    let Some(home) = files::ops::orca_home() else {
        return;
    };
    let path = home.join("pending_restart");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = format!("{target}\n{now}\n");
    _ = std::fs::write(&path, body);
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
pub(crate) fn schedule_self_restart() -> &'static str {
    // Pick the restart method first so we can report it back to the caller.
    // On Linux under a system-mode systemd unit, `systemctl restart` requires
    // polkit auth that an unprivileged `User=orca` daemon does NOT have — the
    // call returns "Access denied" and the daemon keeps running the stale
    // binary. Self-SIGTERM is privilege-free and, paired with `Restart=always`
    // in the unit, gets the same outcome.
    let my_pid = std::process::id();
    let (method, cmd): (&'static str, String);

    #[cfg(target_os = "macos")]
    {
        method = "launchctl-kickstart-or-self-sigterm";
        cmd = format!(
            "sleep 2; if launchctl list 2>/dev/null | grep -q com.orca.daemon; then \
                 launchctl kickstart -k gui/$(id -u)/com.orca.daemon; \
             else kill -TERM {my_pid}; fi"
        );
    }
    #[cfg(target_os = "linux")]
    {
        // Detect supervisor for reporting; the action itself is always self-
        // SIGTERM (works regardless of user/system mode, and respawn is
        // owned by the supervisor's `Restart=always`).
        let supervised = std::path::Path::new("/run/systemd/system").exists();
        if is_unraid() {
            // Unraid: the .plg install script wraps the daemon launch in a
            // respawn loop (see `render_plg_install_script` in package.rs).
            // Self-SIGTERM kills the inner `orca daemon`; the wrapper's `while`
            // re-execs APPDATA/bin/orca, picking up the just-swapped binary.
            // Retired the `/etc/rc.d/rc.orca restart` path 2026-06-06 along
            // with the rc.orca script itself — see
            // [[project-unraid-rc-orca-stale-pid-race]].
            method = "unraid-plg-respawn";
            cmd = format!("sleep 2; kill -TERM {my_pid}");
        } else {
            method = if supervised {
                "systemd-self-sigterm"
            } else {
                "unsupervised-self-sigterm"
            };
            cmd = format!("sleep 2; kill -TERM {my_pid}");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        method = "self-sigterm";
        cmd = format!("sleep 2; kill -TERM {my_pid}");
    }

    let spawned = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok();
    if !spawned {
        return "spawn-failed";
    }
    method
}

/// Read the pending-restart marker written by [`apply_update`]. Returns
/// `(target_version, age_seconds)` if present, else `None`.
///
/// Callers in the tool response use this to surface "applied but daemon
/// did not actually restart" — a class of failure that was previously
/// silent (apply returns OK, in-process binary swap succeeds, but the
/// supervisor never relaunches so `current_version` keeps reporting the
/// stale compile-time constant).
pub fn read_pending_restart() -> Option<(String, u64)> {
    let home = files::ops::orca_home()?;
    let raw = std::fs::read_to_string(home.join("pending_restart")).ok()?;
    let mut lines = raw.lines();
    let target = lines.next()?.trim().to_string();
    let ts: u64 = lines.next()?.trim().parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let age = now.saturating_sub(ts);
    Some((target, age))
}

/// Best-effort: clear the pending-restart marker. Called on daemon startup
/// once the running version matches the target — i.e., the restart took.
pub fn clear_pending_restart() {
    if let Some(home) = files::ops::orca_home() {
        _ = std::fs::remove_file(home.join("pending_restart"));
    }
}

/// Resolve a release tag + target triple to a verified binary blob.
///
/// Looks up the GitHub release for `v_tag` (with or without `v` prefix),
/// finds the asset named `orca-<version>-<target>` (or legacy
/// `orca-<target>`), downloads the asset + `.sha256` checksum, and verifies
/// the asset against the checksum. Returns `(bytes, sha256_hex, version)`.
///
/// `target` is an explicit Rust target triple (`x86_64-unknown-linux-gnu`,
/// `aarch64-apple-darwin`, etc.) — the caller may be on a different arch
/// from the host that holds the GitHub token. This is the engine for the
/// peer-dispatched `system.fetch_release_asset` tool (delegate-on-miss).
pub async fn fetch_release_asset(
    v_tag: &str,
    target: &str,
    token: &str,
) -> Result<(Vec<u8>, String, String)> {
    if token.is_empty() {
        bail!("no github token available — set secret 'github_token' or export GITHUB_TOKEN");
    }
    let v_tag = if v_tag.starts_with('v') {
        v_tag.to_string()
    } else {
        format!("v{v_tag}")
    };
    let client = utils::http::Client::new();
    let user_agent = format!("{APP_NAME}/{CURRENT_VERSION}");
    let url = format!("{APP_REPO_API_URL}/releases/tags/{v_tag}");
    let resp = client
        .get(url)
        .bearer(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header("User-Agent", &user_agent)
        .send()
        .await
        .with_context(|| format!("fetch release {v_tag}"))?;
    let release: Release = resp.json().context("parse release json")?;
    let stripped = release.tag_name.trim_start_matches('v').to_string();
    let (asset_name, asset_url) =
        select_asset(&release.assets, &stripped, target).with_context(|| {
            format!(
                "no asset for {v_tag} matching {} or {}",
                versioned_asset_name(&stripped, target),
                legacy_asset_name(target)
            )
        })?;
    let checksum_name = format!("{asset_name}.sha256");
    let checksum_url = select_checksum_url(&release.assets, &checksum_name)
        .with_context(|| format!("no checksum asset {checksum_name} for {v_tag}"))?;

    let cs_bytes = download_asset(&client, &checksum_url, token).await?;
    let cs_str = String::from_utf8_lossy(&cs_bytes);
    let expected = cs_str
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
        .with_context(|| format!("checksum file empty at {checksum_url}"))?;
    let bytes = download_asset(&client, &asset_url, token).await?;
    verify_sha256(&bytes, &expected)?;
    Ok((bytes, expected, stripped))
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

// ── release / asset selection (pure) ───────────────────────────────────────────

/// Versioned asset name: `orca-<version>-<target>` (current pipeline, v0.0.4+).
fn versioned_asset_name(version: &str, target: &str) -> String {
    format!("{APP_NAME}-{version}-{target}")
}

/// Legacy asset name: `orca-<target>` (pipeline ≤ v0.0.3).
fn legacy_asset_name(target: &str) -> String {
    format!("{APP_NAME}-{target}")
}

/// Pick the newest release accepted by `channel` using full semver ordering
/// (handles `-rc` suffixes) so an rc.15 tag doesn't get out-ranked by a stale
/// stable v0.0.2. Returns None when no release passes the channel filter.
fn pick_best_release(releases: Vec<Release>, channel: &Channel) -> Option<Release> {
    releases
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
        })
}

/// Choose the release asset for `version`/`target`, preferring the versioned
/// name over the legacy name so a re-issued release carrying both resolves to
/// the canonical one. Returns `(asset_name, asset_url)`.
fn select_asset(assets: &[Asset], version: &str, target: &str) -> Option<(String, String)> {
    let versioned = versioned_asset_name(version, target);
    let legacy = legacy_asset_name(target);
    assets
        .iter()
        .find(|a| a.name == versioned)
        .or_else(|| assets.iter().find(|a| a.name == legacy))
        .map(|a| (a.name.clone(), a.url.clone()))
}

/// Find the checksum asset URL by exact name (`<asset>.sha256`).
fn select_checksum_url(assets: &[Asset], checksum_name: &str) -> Option<String> {
    assets
        .iter()
        .find(|a| a.name == checksum_name)
        .map(|a| a.url.clone())
}

// ── sha256 helpers ────────────────────────────────────────────────────────────

/// Verify a freshly-written binary on disk:
///   1. file size matches the expected byte count
///   2. sha256 of the file contents matches the expected hash
///   3. exec'ing `<path> --version` prints the expected version string
///
/// Fails closed — any check that can't run is treated as a verification
/// failure so a silent filesystem fault (FUSE truncation, partial write,
/// permission flip) cannot masquerade as a successful update.
pub fn verify_on_disk(path: &std::path::Path, expected_bytes: &[u8], version: &str) -> Result<()> {
    let on_disk = std::fs::read(path)
        .with_context(|| format!("read back {} for verification", path.display()))?;
    if on_disk.len() != expected_bytes.len() {
        bail!(
            "size mismatch at {}: expected {} bytes, got {}",
            path.display(),
            expected_bytes.len(),
            on_disk.len()
        );
    }
    let expected_hash = utils::hash::sha256_hex(expected_bytes);
    let got_hash = utils::hash::sha256_hex(&on_disk);
    if got_hash != expected_hash {
        bail!(
            "sha256 mismatch at {}: expected {}, got {}",
            path.display(),
            expected_hash,
            got_hash
        );
    }
    let out = std::process::Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("exec {} --version", path.display()))?;
    if !out.status.success() {
        bail!("{} --version exited {}", path.display(), out.status);
    }
    let printed = String::from_utf8_lossy(&out.stdout);
    if !printed.contains(version) {
        bail!(
            "{} --version printed {:?}, expected to contain {:?}",
            path.display(),
            printed.trim(),
            version
        );
    }
    Ok(())
}

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
    Some(files::ops::orca_home()?.join("cache").join("sha256"))
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
        // A file with a future mtime (clock skew, a touch into the future)
        // makes `duration_since` return Err; treat its age as the absolute
        // skew so a genuinely stale entry past the TTL is still pruned
        // rather than living forever.
        let age = now
            .duration_since(modified)
            .unwrap_or_else(|e| e.duration());
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

    // ── asset-name construction ───────────────────────────────────────────────

    #[test]
    fn versioned_asset_name_shape() {
        assert_eq!(
            versioned_asset_name("0.0.4", "x86_64-unknown-linux-gnu"),
            "orca-0.0.4-x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn legacy_asset_name_shape() {
        assert_eq!(
            legacy_asset_name("aarch64-apple-darwin"),
            "orca-aarch64-apple-darwin"
        );
    }

    // ── select_asset ──────────────────────────────────────────────────────────

    fn asset(name: &str, url: &str) -> Asset {
        Asset {
            name: name.to_string(),
            url: url.to_string(),
        }
    }

    fn release(tag: &str, assets: Vec<Asset>) -> Release {
        Release {
            tag_name: tag.to_string(),
            assets,
        }
    }

    #[test]
    fn select_asset_prefers_versioned_over_legacy() {
        let t = "x86_64-unknown-linux-gnu";
        let assets = vec![
            asset(&legacy_asset_name(t), "legacy-url"),
            asset(&versioned_asset_name("0.0.4", t), "versioned-url"),
        ];
        let (name, url) = select_asset(&assets, "0.0.4", t).expect("asset found");
        assert_eq!(name, versioned_asset_name("0.0.4", t));
        assert_eq!(url, "versioned-url");
    }

    #[test]
    fn select_asset_falls_back_to_legacy() {
        let t = "x86_64-unknown-linux-gnu";
        let assets = vec![asset(&legacy_asset_name(t), "legacy-url")];
        let (name, url) = select_asset(&assets, "0.0.4", t).expect("asset found");
        assert_eq!(name, legacy_asset_name(t));
        assert_eq!(url, "legacy-url");
    }

    #[test]
    fn select_asset_none_when_target_mismatch() {
        let assets = vec![
            asset("orca-0.0.4-aarch64-apple-darwin", "u1"),
            asset("orca-aarch64-apple-darwin", "u2"),
        ];
        assert!(select_asset(&assets, "0.0.4", "x86_64-unknown-linux-gnu").is_none());
    }

    #[test]
    fn select_asset_none_when_empty() {
        assert!(select_asset(&[], "0.0.4", "x86_64-unknown-linux-gnu").is_none());
    }

    // ── select_checksum_url ───────────────────────────────────────────────────

    #[test]
    fn select_checksum_url_matches_exact_name() {
        let assets = vec![
            asset("orca-0.0.4-x86_64-unknown-linux-gnu", "bin-url"),
            asset("orca-0.0.4-x86_64-unknown-linux-gnu.sha256", "cs-url"),
        ];
        assert_eq!(
            select_checksum_url(&assets, "orca-0.0.4-x86_64-unknown-linux-gnu.sha256"),
            Some("cs-url".to_string())
        );
    }

    #[test]
    fn select_checksum_url_none_when_absent() {
        let assets = vec![asset("orca-0.0.4-x86_64-unknown-linux-gnu", "bin-url")];
        assert!(
            select_checksum_url(&assets, "orca-0.0.4-x86_64-unknown-linux-gnu.sha256").is_none()
        );
    }

    // ── pick_best_release ─────────────────────────────────────────────────────

    #[test]
    fn pick_best_release_rc_beats_stale_stable_on_rc_channel() {
        let releases = vec![
            release("v0.0.2", vec![]),
            release("v0.0.4-rc.15", vec![]),
            release("v0.0.4-rc.3", vec![]),
        ];
        let best = pick_best_release(releases, &Channel::Rc).expect("some release");
        assert_eq!(best.tag_name, "v0.0.4-rc.15");
    }

    #[test]
    fn pick_best_release_stable_channel_skips_rc_tags() {
        let releases = vec![release("v0.0.4-rc.15", vec![]), release("v0.0.3", vec![])];
        let best = pick_best_release(releases, &Channel::Stable).expect("some release");
        assert_eq!(best.tag_name, "v0.0.3");
    }

    #[test]
    fn pick_best_release_none_when_all_filtered() {
        let releases = vec![release("v0.0.4-rc.1", vec![])];
        assert!(pick_best_release(releases, &Channel::Stable).is_none());
    }

    #[test]
    fn pick_best_release_stable_outranks_matching_rc() {
        // Full semver: stable core beats rc of same core.
        let releases = vec![release("v0.0.4-rc.9", vec![]), release("v0.0.4", vec![])];
        let best = pick_best_release(releases, &Channel::Rc).expect("some release");
        assert_eq!(best.tag_name, "v0.0.4");
    }

    #[test]
    fn pick_best_release_empty_input() {
        assert!(pick_best_release(vec![], &Channel::Rc).is_none());
    }

    // ── Release / ReleaseMeta JSON parsing ────────────────────────────────────

    #[test]
    fn release_deserializes_from_github_json() {
        let json = r#"{
            "tag_name": "v0.0.4-rc.3",
            "assets": [
                {"name": "orca-0.0.4-rc.3-x86_64-unknown-linux-gnu", "url": "https://api/asset/1"},
                {"name": "orca-0.0.4-rc.3-x86_64-unknown-linux-gnu.sha256", "url": "https://api/asset/2"}
            ]
        }"#;
        let r: Release = serde_json::from_str(json).expect("parse");
        assert_eq!(r.tag_name, "v0.0.4-rc.3");
        assert_eq!(r.assets.len(), 2);
        let (name, url) =
            select_asset(&r.assets, "0.0.4-rc.3", "x86_64-unknown-linux-gnu").expect("asset");
        assert_eq!(name, "orca-0.0.4-rc.3-x86_64-unknown-linux-gnu");
        assert_eq!(url, "https://api/asset/1");
    }

    #[test]
    fn release_meta_defaults_fill_missing_fields() {
        // published_at + prerelease absent → serde defaults apply.
        let json = r#"{"tag_name": "v0.0.4"}"#;
        let m: ReleaseMeta = serde_json::from_str(json).expect("parse");
        assert_eq!(m.tag_name, "v0.0.4");
        assert!(!m.prerelease);
        assert!(m.published_at.is_none());
    }

    // ── pending-restart marker round-trip ─────────────────────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn pending_restart_marker_round_trips() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }

        assert!(read_pending_restart().is_none());
        write_pending_restart_marker("0.0.42");
        let (target, age) = read_pending_restart().expect("marker present");
        assert_eq!(target, "0.0.42");
        assert!(age < 60, "age should be near-zero, got {age}");

        clear_pending_restart();
        assert!(read_pending_restart().is_none());

        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_pending_restart_none_on_malformed_marker() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }

        // Single line — missing timestamp line → parse fails → None.
        std::fs::write(tmp.path().join("pending_restart"), "0.0.42\n").expect("write");
        assert!(read_pending_restart().is_none());

        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    // ── cached sha256 write + path ────────────────────────────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn write_cached_sha256_creates_file_at_expected_path() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }

        let path = write_cached_sha256("0.0.7", b"deadbeef  orca\n").expect("write");
        let expected = tmp.path().join("cache").join("sha256").join("0.0.7.sha256");
        assert_eq!(path, expected);
        assert_eq!(std::fs::read(&path).unwrap(), b"deadbeef  orca\n");

        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    // ── verify_on_disk size guard (no exec needed) ────────────────────────────

    #[test]
    fn verify_on_disk_size_mismatch_bails() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"short").expect("write");
        let err = verify_on_disk(&path, b"much longer expected bytes", "0.0.1").unwrap_err();
        assert!(err.to_string().contains("size mismatch"), "{err}");
    }

    #[test]
    fn verify_on_disk_missing_file_bails() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("does-not-exist");
        let err = verify_on_disk(&path, b"bytes", "0.0.1").unwrap_err();
        assert!(err.to_string().contains("read back"), "{err}");
    }

    #[test]
    fn verify_on_disk_hash_mismatch_bails() {
        // Same length as expected bytes so we get past the size check and hit
        // the sha256 comparison.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("blob");
        std::fs::write(&path, b"AAAAA").expect("write");
        let err = verify_on_disk(&path, b"BBBBB", "0.0.1").unwrap_err();
        assert!(err.to_string().contains("sha256 mismatch"), "{err}");
    }

    // ── build_target accessor ─────────────────────────────────────────────────

    #[test]
    fn build_target_is_nonempty() {
        assert!(!build_target().is_empty());
        assert_eq!(build_target(), BUILD_TARGET);
    }

    // ── asset-name construction (more targets) ────────────────────────────────

    #[test]
    fn versioned_asset_name_embeds_version_and_target() {
        assert_eq!(
            versioned_asset_name("1.2.3-rc.4", "aarch64-unknown-linux-musl"),
            "orca-1.2.3-rc.4-aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn checksum_name_is_asset_plus_suffix() {
        let asset = versioned_asset_name("0.0.4", "x86_64-apple-darwin");
        assert_eq!(
            format!("{asset}.sha256"),
            "orca-0.0.4-x86_64-apple-darwin.sha256"
        );
    }

    // ── select_asset: versioned present, legacy absent ────────────────────────

    #[test]
    fn select_asset_versioned_only_resolves() {
        let t = "aarch64-apple-darwin";
        let assets = vec![asset(&versioned_asset_name("0.0.5", t), "v-url")];
        let (name, url) = select_asset(&assets, "0.0.5", t).expect("asset");
        assert_eq!(name, versioned_asset_name("0.0.5", t));
        assert_eq!(url, "v-url");
    }

    #[test]
    fn select_asset_ignores_wrong_version_versioned_name() {
        let t = "x86_64-unknown-linux-gnu";
        // Only a differently-versioned asset present, no legacy → no match.
        let assets = vec![asset(&versioned_asset_name("0.0.3", t), "u")];
        assert!(select_asset(&assets, "0.0.4", t).is_none());
    }

    // ── VersionEntry serde/default ────────────────────────────────────────────

    #[test]
    fn version_entry_default_is_empty() {
        let e = VersionEntry::default();
        assert!(e.tag.is_empty());
        assert!(!e.prerelease);
        assert!(e.published_at.is_none());
        assert!(!e.is_current);
    }

    #[test]
    fn version_entry_roundtrips_json() {
        let e = VersionEntry {
            tag: "v0.0.4".into(),
            prerelease: false,
            published_at: Some("2026-01-01T00:00:00Z".into()),
            is_current: true,
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: VersionEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tag, "v0.0.4");
        assert!(back.is_current);
        assert_eq!(back.published_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn version_entry_fills_defaults_from_partial_json() {
        // #[serde(default)] on the struct means absent fields default.
        let e: VersionEntry = serde_json::from_str(r#"{"tag":"v1.0.0"}"#).unwrap();
        assert_eq!(e.tag, "v1.0.0");
        assert!(!e.prerelease);
        assert!(e.published_at.is_none());
        assert!(!e.is_current);
    }

    // ── ReleaseMeta prerelease parsing ────────────────────────────────────────

    #[test]
    fn release_meta_parses_prerelease_true() {
        let json =
            r#"{"tag_name":"v0.0.4-rc.1","prerelease":true,"published_at":"2026-02-02T00:00:00Z"}"#;
        let m: ReleaseMeta = serde_json::from_str(json).expect("parse");
        assert_eq!(m.tag_name, "v0.0.4-rc.1");
        assert!(m.prerelease);
        assert_eq!(m.published_at.as_deref(), Some("2026-02-02T00:00:00Z"));
    }

    // ── pick_best_release: equal tags, single element ─────────────────────────

    #[test]
    fn pick_best_release_single_accepted_release() {
        let releases = vec![release("v0.0.4", vec![])];
        let best = pick_best_release(releases, &Channel::Stable).expect("some");
        assert_eq!(best.tag_name, "v0.0.4");
    }

    #[test]
    fn pick_best_release_handles_v_prefix_and_bare_equally() {
        // Both accepted on Rc; newest wins regardless of `v` prefix presence.
        let releases = vec![
            release("0.0.4-rc.1", vec![]),
            release("v0.0.4-rc.2", vec![]),
        ];
        let best = pick_best_release(releases, &Channel::Rc).expect("some");
        assert_eq!(best.tag_name, "v0.0.4-rc.2");
    }

    // ── read_pending_restart edge cases ───────────────────────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn read_pending_restart_none_when_marker_absent() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        assert!(read_pending_restart().is_none());
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_pending_restart_computes_age_from_past_timestamp() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let ten_min_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 600;
        std::fs::write(
            tmp.path().join("pending_restart"),
            format!("0.0.50\n{ten_min_ago}\n"),
        )
        .unwrap();
        let (target, age) = read_pending_restart().expect("marker");
        assert_eq!(target, "0.0.50");
        assert!((590..=610).contains(&age), "age ~600s, got {age}");
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn read_pending_restart_none_on_non_numeric_timestamp() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        std::fs::write(tmp.path().join("pending_restart"), "0.0.50\nnotanumber\n").unwrap();
        assert!(read_pending_restart().is_none());
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn write_pending_restart_marker_body_shape() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: ORCA_HOME-touching tests serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        write_pending_restart_marker("9.9.9");
        let body = std::fs::read_to_string(tmp.path().join("pending_restart")).unwrap();
        let mut lines = body.lines();
        assert_eq!(lines.next(), Some("9.9.9"));
        // Second line parses as a plausible unix timestamp.
        assert!(lines.next().unwrap().parse::<u64>().unwrap() > 1_600_000_000);
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    // ── prune_check_cache: keeps fresh, drops stale ───────────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn prune_check_cache_keeps_fresh_and_drops_stale() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let dir = check_cache_dir().expect("cache dir");
        std::fs::create_dir_all(&dir).unwrap();

        let fresh = dir.join("fresh.sha256");
        std::fs::write(&fresh, b"x").unwrap();

        let stale = dir.join("stale.sha256");
        std::fs::write(&stale, b"x").unwrap();
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_secs(CHECK_CACHE_TTL_SECS + 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        prune_check_cache();

        assert!(fresh.exists(), "fresh entry kept");
        assert!(!stale.exists(), "stale entry pruned");
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    #[test]
    #[serial_test::serial(env)]
    fn prune_check_cache_no_dir_is_noop() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        // Cache dir never created — prune must not panic.
        prune_check_cache();
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    // ── cached_sha256_path naming ─────────────────────────────────────────────

    #[test]
    #[serial_test::serial(env)]
    fn cached_sha256_path_uses_version_filename() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }
        let p = cached_sha256_path("0.1.2").expect("path");
        assert_eq!(
            p,
            tmp.path().join("cache").join("sha256").join("0.1.2.sha256")
        );
        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }

    /// A cached sha256 whose mtime is in the future by more than the TTL
    /// (clock skew, a stray `touch -d` into the future) used to be skipped
    /// forever because `duration_since` returned Err and the loop did
    /// `continue`. The future-mtime fix treats the skew as the file's age,
    /// so an entry beyond the TTL is pruned regardless of clock direction.
    #[test]
    #[serial_test::serial(env)]
    fn prune_check_cache_removes_future_mtime_past_ttl() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // SAFETY: tests touching ORCA_HOME are serialized via #[serial(env)].
        unsafe {
            std::env::set_var("ORCA_HOME", tmp.path());
        }

        let dir = check_cache_dir().expect("cache dir resolves");
        std::fs::create_dir_all(&dir).expect("mkdir cache dir");
        let target = dir.join("0.0.99.sha256");
        std::fs::write(&target, b"deadbeef").expect("write cache file");

        // Push the mtime well past the TTL into the future.
        let future = std::time::SystemTime::now()
            + std::time::Duration::from_secs(CHECK_CACHE_TTL_SECS + 86_400);
        std::fs::File::options()
            .write(true)
            .open(&target)
            .expect("reopen for set_modified")
            .set_modified(future)
            .expect("set future mtime");
        assert!(target.exists(), "precondition: file present before prune");

        prune_check_cache();

        assert!(
            !target.exists(),
            "future-mtime entry past TTL should be pruned"
        );

        unsafe {
            std::env::remove_var("ORCA_HOME");
        }
    }
}
