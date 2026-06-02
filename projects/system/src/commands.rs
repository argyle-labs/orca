//! Lifecycle tool surface: install / delete + the unified `system.update` tool
//! that owns every system-update concern (orca binary, channel, pin,
//! dev-source, hostname/fqdn, addressing overrides, OS package upgrade).
//!
//! Per [[feedback-one-tool-per-resource]] there is exactly ONE `system.update`
//! — never a `system.update.apply` / `.pin` / `.unpin` / `host.set` family.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dev::{
    apply_update_dev, check_for_update_dev, clear_dev_source, read_dev_source, write_dev_source,
};
use crate::install::{InstallReport, cmd_install_report, cmd_uninstall_report};
use crate::update::{
    UpdateInfo, VersionEntry, apply_update, check_for_update, list_versions, prune_check_cache,
    resolve_github_token,
};
use crate::update_state::{
    Channel, clear_version_pin, read_channel_marker, read_version_pin, resolve_pin_veto,
    write_channel_marker, write_version_pin,
};
use derive::orca_tool;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── shared args ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

// ── install / delete ───────────────────────────────────────────────────────

/// Args for [`system_install`]. Empty by default — does the user-level
/// install. Pass `service_user` (and optional `home_dir` / `admin_pubkey`)
/// to also provision a system service user with SSH access (Linux, root).
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemInstallArgs {
    /// Service user name. When set, also runs the service-user bootstrap
    /// (`useradd`, group membership, linger, optional SSH key). Linux-only;
    /// no-op on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub service_user: Option<String>,
    /// Home directory for the service user (default: `/var/lib/orca`).
    /// Ignored when `service_user` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub home_dir: Option<String>,
    /// SSH pubkey to append to the service user's `authorized_keys`.
    /// Ignored when `service_user` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub admin_pubkey: Option<String>,
    /// HTTP port the daemon supervisor should bind. Defaults to the
    /// workspace-wide `APP_REST_HTTP_PORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub port: Option<u16>,
}

/// [MUTATES STATE] Install orca on this host. Always wires the user-level
/// install (binary, ~/.claude symlinks, MCP registration, PKI). When
/// `service_user` is set, also bootstraps a system service user with SSH
/// access — replaces the former separate `system.bootstrap` tool.
#[orca_tool(domain = "system", verb = "install", local_only = true)]
async fn system_install(
    args: SystemInstallArgs,
    _ctx: &contract::ToolCtx,
) -> Result<InstallReport> {
    let mut report = cmd_install_report();
    if let Some(user) = &args.service_user {
        let home = args
            .home_dir
            .as_deref()
            .unwrap_or(crate::sysadmin::DEFAULT_SERVICE_HOME);
        match crate::sysadmin::bootstrap(args.admin_pubkey.clone(), user, home) {
            Ok(()) => report
                .done
                .push(format!("service user '{user}' (home: {home})")),
            Err(e) => report
                .errors
                .push(format!("service-user bootstrap failed: {e}")),
        }
    }
    let port = args.port.unwrap_or(crate::daemon::DEFAULT_HTTP_PORT);
    match crate::daemon::install(port, args.service_user.clone()) {
        Ok(()) => report
            .done
            .push(format!("daemon supervisor installed on port {port}")),
        Err(e) => report
            .errors
            .push(format!("daemon supervisor install failed: {e}")),
    }
    Ok(report)
}

/// [MUTATES STATE] Uninstall orca from this host: remove binary, MCP
/// registration, CLAUDE.md symlinks, AND the daemon supervisor unit
/// (launchd / systemd / openrc / unraid). Absorbed the former
/// `system.daemon.uninstall`.
#[orca_tool(domain = "system", verb = "delete", local_only = true)]
async fn system_delete(_args: EmptyArgs, _ctx: &contract::ToolCtx) -> Result<InstallReport> {
    let mut report = cmd_uninstall_report();
    match crate::daemon::uninstall_service() {
        Ok(()) => report.done.push("daemon supervisor removed".to_string()),
        Err(e) => report
            .errors
            .push(format!("daemon supervisor removal failed: {e}")),
    }
    Ok(report)
}

// ── system.update — the one tool ───────────────────────────────────────────

/// Args for [`system_update`]. Every field is optional; omit-all = read-only
/// state probe (returns current_version / channel / pinned_to / available_versions).
///
/// One tool, many surfaces:
///   - orca binary: `channel`, `version`, `pin`, `unpin`, `dev_source`, `clear_dev_source`
///   - system identity: `hostname`, `fqdn`
///   - addressing overrides: `lan_v4`, `lan_v6`, `tailscale_v4`, `tailscale_v6`
///   - OS package upgrade: `os_packages`
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemUpdateArgs {
    /// Switch update channel: stable | rc | dev. On change, applies latest on the new channel.
    /// `dev` enables dev mode (tracks GitHub HEAD via cargo-watch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub channel: Option<String>,

    /// Apply a specific version (semver, leading `v` optional). Does NOT pin unless `--pin` is also set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub version: Option<String>,

    /// Pin to the version about to be applied (or to `version` if given). Future updates won't cross it.
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(long))]
    pub pin: bool,

    /// Clear the version pin.
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(long))]
    pub unpin: bool,

    /// Set the dev-source URL (orca fetches binaries from there instead of GitHub).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub dev_source: Option<String>,

    /// Clear the dev-source URL.
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(long))]
    pub clear_dev_source: bool,

    /// Change this host's OS hostname. Also updates `host.display_name` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub hostname: Option<String>,

    /// Set the host's FQDN setting (no DNS write — UI/peer-display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub fqdn: Option<String>,

    /// Manual LAN IPv4 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub lan_v4: Option<String>,

    /// Manual LAN IPv6 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub lan_v6: Option<String>,

    /// Manual Tailscale IPv4 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub tailscale_v4: Option<String>,

    /// Manual Tailscale IPv6 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub tailscale_v6: Option<String>,

    /// Run the OS package upgrade (apt / apk / brew / unraid plugin).
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(long))]
    pub os_packages: bool,

    /// Force a re-detect of host addressing channels (LAN + Tailscale +
    /// settings overrides). Was `system.host.refresh`. Drives the
    /// `HostRefreshHook` registered at server startup.
    #[serde(default)]
    #[cfg_attr(feature = "cli", arg(long))]
    pub refresh_host: bool,

    /// Daemon action: "stop" (SIGTERM), "park" (SIGUSR1, release port),
    /// or "reclaim" (SIGUSR2, take port back). Was the
    /// `system.daemon.{stop,park,reclaim}` family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub daemon: Option<String>,
}

/// Result of a `system.update` call.
///
/// Every field carries `#[serde(default)]` so a controller running rc.N can
/// decode a response from a peer running rc.N-1 even when the older peer
/// omits a field that was added later. Without this, a single missing field
/// would fail the entire decode and the controller would report failure for
/// a call that actually applied successfully on the peer. See
/// [[project-update-path-fix-plan-2026-06-01]] fix #1.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Default)]
#[serde(default)]
pub struct SystemUpdateOutput {
    pub current_version: String,
    pub channel: String,
    pub pinned_to: Option<String>,
    pub dev_source: Option<String>,
    pub available_versions: Vec<VersionEntry>,
    pub latest: Option<String>,
    pub applied: Option<String>,
    pub hostname: Option<String>,
    pub fqdn: Option<String>,
    pub addressing_set: Vec<String>,
    pub os_package_result: Option<String>,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

/// [MUTATES STATE] The single system-update tool. Covers orca binary updates,
/// host identity (hostname/fqdn/addressing), and OS package upgrades. Omit
/// every arg for a read-only state probe.
#[orca_tool(domain = "system", verb = "update", refresh_runtime = true)]
async fn system_update(
    args: SystemUpdateArgs,
    ctx: &contract::ToolCtx,
) -> Result<SystemUpdateOutput> {
    prune_check_cache();

    let mut notes: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut addressing_set: Vec<String> = Vec::new();
    let mut hostname_applied: Option<String> = None;
    let mut fqdn_applied: Option<String> = None;
    let mut os_package_result: Option<String> = None;
    let mut applied: Option<String> = None;

    // ── 1. config-only mutations ────────────────────────────────────────────
    let mut channel_changed = false;
    let mut dev_mode_requested = false;
    if let Some(raw) = args
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let prior = read_channel_marker().unwrap_or(Channel::Stable);
        if raw == "dev" {
            dev_mode_requested = true;
            let ch = Channel::Dev;
            write_channel_marker(&ch).context("write channel marker")?;
            notes.push(
                "channel set to dev — run `orca dev enable` to start the cargo-watch supervisor"
                    .into(),
            );
        } else {
            let ch = Channel::parse(raw);
            write_channel_marker(&ch).context("write channel marker")?;
            if ch != prior {
                channel_changed = true;
                notes.push(format!("channel set to {}", ch.as_marker()));
            }
        }
    }
    if args.unpin {
        clear_version_pin().context("clear version pin")?;
        notes.push("pin cleared".into());
    }
    if let Some(src) = args
        .dev_source
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        write_dev_source(src).context("write dev source")?;
        notes.push(format!("dev source set to {src}"));
    }
    if args.clear_dev_source {
        clear_dev_source().context("clear dev source")?;
        notes.push("dev source cleared".into());
    }

    // ── 2. host identity ───────────────────────────────────────────────────
    if let Some(name) = args
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match set_os_hostname(name).await {
            Ok(()) => {
                if let Ok(conn) = db::open_default()
                    && let Err(e) = db::settings::set(&conn, "host.display_name", name)
                {
                    errors.push(format!("write host.display_name setting: {e}"));
                }
                hostname_applied = Some(name.to_string());
                notes.push(format!("hostname set to {name}"));
            }
            Err(e) => errors.push(format!("hostname set failed: {e}")),
        }
    }
    if let Some(v) = args
        .fqdn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match db::open_default().and_then(|c| db::settings::set(&c, "host.fqdn", v)) {
            Ok(()) => {
                fqdn_applied = Some(v.to_string());
                notes.push(format!("fqdn set to {v}"));
            }
            Err(e) => errors.push(format!("fqdn set failed: {e}")),
        }
    }
    for (label, val) in [
        ("lan_v4", args.lan_v4.as_deref()),
        ("lan_v6", args.lan_v6.as_deref()),
        ("tailscale_v4", args.tailscale_v4.as_deref()),
        ("tailscale_v6", args.tailscale_v6.as_deref()),
    ] {
        if let Some(v) = val.map(str::trim).filter(|s| !s.is_empty()) {
            match db::open_default()
                .and_then(|c| db::host_addressing::upsert_host_addressing(&c, label, v, "manual"))
            {
                Ok(()) => {
                    addressing_set.push(format!("{label}={v}"));
                    notes.push(format!("{label} override = {v}"));
                }
                Err(e) => errors.push(format!("{label} set failed: {e}")),
            }
        }
    }

    // ── 3a. daemon signal (was `system.daemon.{stop,park,reclaim}`) ───────
    if let Some(action) = args.daemon.as_deref() {
        let result = match action {
            "stop" => crate::daemon::stop().map(|pid| format!("daemon stop sent (pid {pid})")),
            "park" => crate::daemon::park().map(|pid| format!("daemon parked (pid {pid})")),
            "reclaim" => {
                crate::daemon::reclaim().map(|pid| format!("daemon reclaim sent (pid {pid})"))
            }
            other => Err(anyhow::anyhow!(
                "daemon action '{other}' not one of: stop|park|reclaim"
            )),
        };
        match result {
            Ok(msg) => notes.push(msg),
            Err(e) => errors.push(format!("daemon action failed: {e}")),
        }
    }

    // ── 3b. host-addressing refresh (was `system.host.refresh`) ───────────
    if args.refresh_host {
        match db::open_default() {
            Ok(conn) => {
                if let Ok(hook) =
                    ctx.service::<std::sync::Arc<dyn crate::host::HostRefreshHook + Send + Sync>>()
                    && let Err(e) = hook.refresh(&conn)
                {
                    errors.push(format!("host refresh hook failed: {e}"));
                }
                notes.push("host addressing channels re-detected".to_string());
            }
            Err(e) => errors.push(format!("host refresh db open failed: {e}")),
        }
    }

    // ── 3. OS package upgrade ──────────────────────────────────────────────
    if args.os_packages {
        match run_os_package_update().await {
            Ok(out) => {
                notes.push(format!("os packages: {out}"));
                os_package_result = Some(out);
            }
            Err(e) => errors.push(format!("os packages failed: {e}")),
        }
    }

    // ── 4. orca binary update ──────────────────────────────────────────────
    // Intent: apply binary when (a) version specified, (b) channel changed,
    // or (c) no other mutation requested (default `orca system update`).
    let any_non_binary = args.hostname.is_some()
        || args.fqdn.is_some()
        || args.lan_v4.is_some()
        || args.lan_v6.is_some()
        || args.tailscale_v4.is_some()
        || args.tailscale_v6.is_some()
        || args.os_packages
        || args.refresh_host
        || args.daemon.is_some()
        || args.dev_source.is_some()
        || args.clear_dev_source
        || args.unpin
        || args.pin;
    let binary_intent =
        args.version.is_some() || channel_changed || (!any_non_binary && !dev_mode_requested);

    let ch_marker = read_channel_marker().unwrap_or(Channel::Stable);
    let token = resolve_github_token();

    if binary_intent && !matches!(ch_marker, Channel::Dev) {
        if token.is_empty() && read_dev_source().is_none() {
            errors
                .push("no github token — set secret 'github_token' or export GITHUB_TOKEN".into());
        } else if let Some(src) = read_dev_source() {
            match check_for_update_dev(&src).await {
                Ok(Some(v)) => match apply_update_dev(&src).await {
                    Ok(()) => {
                        applied = Some(v.clone());
                        notes.push(format!("applied dev-source v{v}"));
                    }
                    Err(e) => errors.push(format!("dev-source apply failed: {e}")),
                },
                Ok(None) => notes.push("dev-source: already up to date".into()),
                Err(e) => errors.push(format!("dev-source check failed: {e}")),
            }
        } else if let Some(ver) = args
            .version
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let normalised = normalise_version(ver);
            if args.pin {
                if let Err(e) = write_version_pin(&normalised) {
                    errors.push(format!("pin failed: {e}"));
                } else {
                    notes.push(format!("pinned to {normalised}"));
                }
            }
            match apply_specific_version(&ch_marker, &normalised, &token).await {
                Ok(v) => {
                    applied = Some(v.clone());
                    notes.push(format!("applied v{v}"));
                }
                Err(e) => errors.push(format!("apply v{normalised} failed: {e}")),
            }
        } else {
            match check_for_update(&ch_marker, &token).await {
                Ok(Some(info)) => {
                    if let Some(pin) = resolve_pin_veto(&info.version) {
                        notes.push(format!(
                            "pinned to {pin}; available v{} — pass --unpin to upgrade",
                            info.version
                        ));
                    } else {
                        match apply_update(&info, &token).await {
                            Ok(()) => {
                                if args.pin {
                                    let pin_v = format!("v{}", info.version);
                                    if let Err(e) = write_version_pin(&pin_v) {
                                        errors.push(format!("pin failed: {e}"));
                                    } else {
                                        notes.push(format!("pinned to {pin_v}"));
                                    }
                                }
                                applied = Some(info.version.clone());
                                notes.push(format!("applied v{}", info.version));
                            }
                            Err(e) => errors.push(format!("apply failed: {e}")),
                        }
                    }
                }
                Ok(None) => notes.push(format!("already up to date on {}", ch_marker.as_marker())),
                Err(e) => errors.push(format!("check failed: {e}")),
            }
        }
    }

    // ── 5. probe current state for the response ───────────────────────────
    let available_versions = if matches!(ch_marker, Channel::Dev) || token.is_empty() {
        Vec::new()
    } else {
        match list_versions(&ch_marker, &token).await {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("list versions failed: {e}"));
                Vec::new()
            }
        }
    };
    let latest = available_versions.first().map(|v| v.tag.clone());

    Ok(SystemUpdateOutput {
        current_version: CURRENT_VERSION.to_string(),
        channel: ch_marker.as_marker().to_string(),
        pinned_to: read_version_pin(),
        dev_source: read_dev_source(),
        available_versions,
        latest,
        applied,
        hostname: hostname_applied,
        fqdn: fqdn_applied,
        addressing_set,
        os_package_result,
        notes,
        errors,
    })
}

// ── helpers ────────────────────────────────────────────────────────────────

fn normalise_version(v: &str) -> String {
    if v.starts_with('v') {
        v.to_string()
    } else {
        format!("v{v}")
    }
}

/// Apply a specific version by scanning recent releases for the matching tag.
async fn apply_specific_version(
    channel: &Channel,
    pinned_v_tag: &str, // "v0.0.6-rc.4"
    token: &str,
) -> Result<String> {
    let info = find_release_by_tag(channel, pinned_v_tag, token)
        .await?
        .with_context(|| format!("no release found for {pinned_v_tag}"))?;
    apply_update(&info, token).await?;
    Ok(info.version)
}

async fn find_release_by_tag(
    _channel: &Channel,
    v_tag: &str,
    token: &str,
) -> Result<Option<UpdateInfo>> {
    use contract::config::{APP_NAME, APP_REPO_API_URL};
    if token.is_empty() {
        anyhow::bail!("no github token");
    }
    let url = format!("{APP_REPO_API_URL}/releases/tags/{v_tag}");
    let client = utils::http::Client::new();
    let user_agent = format!("{APP_NAME}/{CURRENT_VERSION}");
    let resp = client
        .get(url)
        .bearer(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &user_agent)
        .send()
        .await
        .context("fetch release by tag")?;
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        assets: Vec<Asset>,
    }
    #[derive(serde::Deserialize)]
    struct Asset {
        name: String,
        url: String,
    }
    let release: Release = resp.json().context("parse release json")?;
    let stripped = release.tag_name.trim_start_matches('v').to_string();
    let build_target = option_env!("ORCA_BUILD_TARGET").unwrap_or("unknown-target");
    let versioned = format!("{APP_NAME}-{stripped}-{build_target}");
    let legacy = format!("{APP_NAME}-{build_target}");
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == versioned)
        .or_else(|| release.assets.iter().find(|a| a.name == legacy))
        .with_context(|| format!("no asset for {v_tag} matching {versioned} or {legacy}"))?;
    let checksum_name = format!("{}.sha256", asset.name);
    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name == checksum_name)
        .map(|a| a.url.clone())
        .with_context(|| format!("no checksum asset {checksum_name} for {v_tag}"))?;
    Ok(Some(UpdateInfo {
        version: stripped,
        asset_url: asset.url.clone(),
        checksum_url,
    }))
}

/// Set the OS hostname. Linux uses `hostnamectl`; macOS uses `scutil`.
async fn set_os_hostname(name: &str) -> Result<()> {
    if name.is_empty() || name.contains(char::is_whitespace) {
        anyhow::bail!("invalid hostname");
    }
    let name = name.to_string();
    tokio::task::spawn_blocking(move || -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let st = std::process::Command::new("hostnamectl")
                .args(["set-hostname", &name])
                .status()
                .context("invoke hostnamectl")?;
            anyhow::ensure!(st.success(), "hostnamectl exited with {st}");
        }
        #[cfg(target_os = "macos")]
        {
            for key in ["HostName", "LocalHostName", "ComputerName"] {
                let st = std::process::Command::new("scutil")
                    .args(["--set", key, &name])
                    .status()
                    .with_context(|| format!("invoke scutil --set {key}"))?;
                anyhow::ensure!(st.success(), "scutil --set {key} exited with {st}");
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = name;
            anyhow::bail!("hostname set unsupported on this platform");
        }
        Ok(())
    })
    .await
    .context("hostname join")?
}

/// Run the OS package upgrade. Detects apt / apk / brew / unraid-plugin.
async fn run_os_package_update() -> Result<String> {
    tokio::task::spawn_blocking(|| -> Result<String> {
        let run = |cmd: &str, args: &[&str]| -> Result<String> {
            let out = std::process::Command::new(cmd)
                .args(args)
                .output()
                .with_context(|| format!("invoke {cmd}"))?;
            let tail = String::from_utf8_lossy(&out.stdout)
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::ensure!(
                out.status.success(),
                "{cmd} exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(tail)
        };
        if which("apt-get") {
            run("apt-get", &["update"])?;
            return run("apt-get", &["upgrade", "-y"]);
        }
        if which("apk") {
            run("apk", &["update"])?;
            return run("apk", &["upgrade"]);
        }
        if which("brew") {
            run("brew", &["update"])?;
            return run("brew", &["upgrade"]);
        }
        anyhow::bail!("no supported package manager found (apt-get/apk/brew)")
    })
    .await
    .context("os package join")?
}

fn which(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {cmd}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── startup notice (called by serve loop) ──────────────────────────────────

/// Non-blocking startup update check — prints a notice, does not download.
pub async fn startup_update_check() {
    let token = resolve_github_token();
    if token.is_empty() {
        return;
    }
    let channel = read_channel_marker().unwrap_or(Channel::Stable);
    if let Ok(Some(info)) = check_for_update(&channel, &token).await {
        if let Some(pin) = resolve_pin_veto(&info.version) {
            println!(
                "[orca] update available: v{} on '{}' (pinned to {pin} — pass --unpin to upgrade)",
                info.version,
                channel.as_marker()
            );
        } else {
            println!(
                "[orca] update available: v{} on '{}' — run `orca system update` to upgrade",
                info.version,
                channel.as_marker()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_version_adds_v_prefix() {
        assert_eq!(normalise_version("0.0.4"), "v0.0.4");
        assert_eq!(normalise_version("v0.0.4"), "v0.0.4");
        assert_eq!(normalise_version("0.0.4-rc.3"), "v0.0.4-rc.3");
    }

    // Simulates an rc.N controller decoding the response payload from a
    // peer running an older rc.N-1 build that omits fields the controller
    // learned about later. Prior to the `#[serde(default)]` attribute a
    // missing field would fail the whole decode and the controller would
    // falsely report the peer's successful apply as a failure. See
    // [[project-update-path-fix-plan-2026-06-01]] fix #1.
    #[test]
    fn system_update_output_decodes_older_peer_response() {
        let older_peer_json = r#"{
            "applied": "v0.0.5-rc.3",
            "notes": ["binary swapped"],
            "errors": []
        }"#;
        let decoded: SystemUpdateOutput = serde_json::from_str(older_peer_json).unwrap();
        assert_eq!(decoded.applied.as_deref(), Some("v0.0.5-rc.3"));
        assert_eq!(decoded.notes, vec!["binary swapped".to_string()]);
        assert!(decoded.errors.is_empty());
        assert!(decoded.current_version.is_empty());
        assert!(decoded.channel.is_empty());
        assert!(decoded.available_versions.is_empty());
    }

    #[test]
    fn system_update_output_decodes_empty_object() {
        let decoded: SystemUpdateOutput = serde_json::from_str("{}").unwrap();
        assert!(decoded.applied.is_none());
        assert!(decoded.errors.is_empty());
    }
}
