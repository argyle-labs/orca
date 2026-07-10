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
    UpdateInfo, VersionEntry, apply_binary, apply_update, build_target, check_for_update,
    fetch_release_asset, list_versions, prune_check_cache, resolve_github_token, verify_sha256,
};
use crate::update_state::{
    Channel, clear_version_pin, read_channel_marker, read_version_pin, resolve_pin_veto,
    write_channel_marker, write_version_pin,
};
use contract::RemoteExec;
use derive::orca_tool;
use std::sync::Arc;

const CURRENT_VERSION: &str = env!("ORCA_VERSION");

// ── shared args ─────────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

// ── install / delete ───────────────────────────────────────────────────────

/// Args for [`system_install`]. Empty by default — does the user-level
/// install. Pass `service_user` (and optional `home_dir` / `admin_pubkey`)
/// to also provision a system service user with SSH access (Linux, root).
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemInstallArgs {
    /// Service user name. When set, also runs the service-user bootstrap
    /// (`useradd`, group membership, linger, optional SSH key). Linux-only;
    /// no-op on macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub service_user: Option<String>,
    /// Home directory for the service user (default: `/var/lib/orca`).
    /// Ignored when `service_user` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub home_dir: Option<String>,
    /// SSH pubkey to append to the service user's `authorized_keys`.
    /// Ignored when `service_user` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub admin_pubkey: Option<String>,
    /// HTTP port the daemon supervisor should bind. Defaults to the
    /// workspace-wide `APP_REST_HTTP_PORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
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

// ── system.serve_release — delegate-on-miss holder side ──────────────────
//
// Peer-dispatchable. A peer whose `github_token` secret is empty calls this
// on a paired peer that DOES hold the token; the holder fetches the release
// from GitHub, verifies the sha256, and returns the binary bytes
// base64-encoded for the JSON-only wire transport. The token never leaves
// the holder. See [[project-github-token-auto-provision]] and
// [[project-secret-delegation-not-distribution]].

/// Args for [`system_fetch_release_asset`].
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct FetchReleaseAssetArgs {
    /// Release tag to fetch, with or without `v` prefix (e.g. `0.0.6-rc.15`
    /// or `v0.0.6-rc.15`). Optional — when omitted the holder resolves the
    /// channel's latest tag using its own GitHub token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub version: Option<String>,
    /// Rust target triple of the requester (e.g. `x86_64-unknown-linux-gnu`,
    /// `aarch64-apple-darwin`). The holder may be on a different arch, so
    /// the caller MUST specify the asset they need.
    #[arg(long)]
    pub target: String,
    /// Channel the requester wants the latest of (`stable` | `rc`). Required
    /// when `version` is omitted; ignored when `version` is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub channel: Option<String>,
}

/// Result of [`system_fetch_release_asset`]. `asset_b64` is base64-STANDARD
/// of the raw binary bytes; `sha256` is the hex digest the holder verified
/// against the release `.sha256` blob (callers MUST re-verify after decode
/// before swapping).
#[derive(Serialize, Deserialize, JsonSchema, Default)]
pub struct FetchReleaseAssetOutput {
    pub asset_b64: String,
    pub sha256: String,
    pub version: String,
}

/// Serve a release asset from GitHub on behalf of a peer that lacks the
/// `github_token` secret. Resolves the token locally, downloads the asset
/// for the requested `target`, verifies sha256 against the release
/// checksum blob, and returns the bytes base64-encoded.
#[orca_tool(domain = "system", verb = "serve_release")]
async fn system_serve_release(
    args: FetchReleaseAssetArgs,
    _ctx: &contract::ToolCtx,
) -> Result<FetchReleaseAssetOutput> {
    let token = resolve_github_token();
    if token.is_empty() {
        anyhow::bail!(
            "this peer has no github_token — cannot serve fetch_release_asset for delegate-on-miss"
        );
    }
    let v_tag = match args
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(v) => v.to_string(),
        None => {
            let ch_name = args.channel.as_deref().unwrap_or("stable");
            let channel = crate::update_state::Channel::parse(ch_name);
            if matches!(channel, crate::update_state::Channel::Dev) {
                anyhow::bail!("channel `dev` has no GitHub releases to fetch");
            }
            let info = crate::update::check_for_update(&channel, &token)
                .await?
                .with_context(|| {
                    format!("channel `{ch_name}` has no release newer than this peer to serve")
                })?;
            info.version
        }
    };
    let (bytes, sha256, version) = fetch_release_asset(&v_tag, &args.target, &token).await?;
    Ok(FetchReleaseAssetOutput {
        asset_b64: utils::encoding::base64_encode(&bytes),
        sha256,
        version,
    })
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
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemUpdateArgs {
    /// Switch update channel: stable | rc | dev. On change, applies latest on the new channel.
    /// `dev` enables dev mode (tracks GitHub HEAD via cargo-watch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub channel: Option<String>,

    /// Apply a specific version (semver, leading `v` optional). Selecting a
    /// non-channel-latest version implicitly pins to that version; selecting
    /// the channel-latest version implicitly unpins. Omit to update to the
    /// channel latest (which also unpins if currently pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub version: Option<String>,

    /// Set the dev-source URL (orca fetches binaries from there instead of GitHub).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub dev_source: Option<String>,

    /// Clear the dev-source URL.
    #[serde(default)]
    #[arg(long)]
    pub clear_dev_source: bool,

    /// Change this host's OS hostname. Also updates `host.display_name` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub hostname: Option<String>,

    /// Set the host's FQDN setting (no DNS write — UI/peer-display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub fqdn: Option<String>,

    /// Manual LAN IPv4 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub lan_v4: Option<String>,

    /// Manual LAN IPv6 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub lan_v6: Option<String>,

    /// Manual Tailscale IPv4 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub tailscale_v4: Option<String>,

    /// Manual Tailscale IPv6 override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub tailscale_v6: Option<String>,

    /// Run the OS package upgrade (apt / apk / brew / unraid plugin).
    #[serde(default)]
    #[arg(long)]
    pub os_packages: bool,

    /// Force a re-detect of host addressing channels (LAN + Tailscale +
    /// settings overrides). Was `system.host.refresh`. Drives the
    /// `HostRefreshHook` registered at server startup.
    #[serde(default)]
    #[arg(long)]
    pub refresh_host: bool,

    /// Daemon action: "stop" (SIGTERM), "park" (SIGUSR1, release port),
    /// or "reclaim" (SIGUSR2, take port back). Was the
    /// `system.daemon.{stop,park,reclaim}` family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
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
    /// Present when a binary swap landed but the daemon has not yet been
    /// observed running the new version. Cleared on daemon startup once
    /// `current_version` matches `target`. Lets remote callers distinguish
    /// "apply succeeded and restarted" from "apply succeeded but supervisor
    /// never restarted us" — the latter previously returned identical
    /// success.
    pub pending_restart: Option<PendingRestart>,
    /// True when `latest` is strictly newer than `current_version` under
    /// semver, ignoring dev-build suffixes (`-dev+g<sha>` and trailing
    /// `.dirty`). Computed server-side so REST/MCP/CLI callers and the
    /// web UI all agree without re-implementing the comparator. `None`
    /// when either side is missing or unparseable.
    pub update_available: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Default, Clone)]
#[serde(default)]
pub struct PendingRestart {
    pub target: String,
    pub age_secs: u64,
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
    if let Some(raw) = args
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let prior = read_channel_marker().unwrap_or(Channel::Stable);
        if raw == "dev" {
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
        || args.clear_dev_source;
    // Per HARD RULE [[feedback-updates-are-user-actions-only]]: an empty
    // `{}` probe MUST NOT apply anything. Per [[task-26-channel-switch-is-
    // filter-only]]: channel is a visibility filter, not an install trigger.
    // Binary install requires an explicit `version` arg — the user clicked
    // Apply on a specific tag. Channel switches persist the marker and
    // return the filtered version list, nothing else.
    let _ = any_non_binary;
    let _ = channel_changed; // marker is written upstream; install intent is version-only now
    let binary_intent = args.version.is_some();

    // Effective channel = max(stored pref, channel implied by running version).
    // If the binary is an rc but the marker says stable (common on hosts
    // installed without explicit channel selection), treat the host as rc
    // for update-check purposes so we don't compare an rc.9 binary against
    // the latest *stable* release and report a phantom "v0.0.5 available".
    //
    // Exception: when the marker is EXPLICITLY set to a non-dev channel,
    // trust it even when running a -dev binary. Without this, hosts deployed
    // from `orca update --source http://<dev>:12009` are stranded forever:
    // implied=Dev forces ch_marker=Dev → list_versions returns [] → the
    // picker is empty → the user can't escape dev. Symptom: echo + alpha
    // on `-dev+gXXXX` builds with no selectable versions.
    let stored_opt = read_channel_marker();
    let stored = stored_opt.unwrap_or(Channel::Stable);
    let implied = Channel::from_version(CURRENT_VERSION);
    let ch_marker = match (stored_opt, implied) {
        (Some(s), Channel::Dev) if !matches!(s, Channel::Dev) => s,
        _ => match (stored, implied) {
            (Channel::Dev, _) | (_, Channel::Dev) => Channel::Dev,
            (Channel::Rc, _) | (_, Channel::Rc) => Channel::Rc,
            _ => Channel::Stable,
        },
    };
    let token = resolve_github_token();

    // Dev-channel gate: dev builds normally don't fetch releases (they
    // track HEAD via cargo-watch). BUT an explicit `--version` is the
    // user telling us "leave dev, go to this tagged build" — honor it.
    // Without this exception, a `-dev` binary silently drops every apply
    // request with no note + no error (the host appears alive but can
    // never be moved off dev via the in-app updater).
    let dev_gate_skip = matches!(ch_marker, Channel::Dev) && args.version.is_none();
    if binary_intent && !dev_gate_skip {
        if token.is_empty() && read_dev_source().is_none() {
            // delegate-on-miss: try paired peers that may hold the token.
            // See [[project-github-token-auto-provision]],
            // [[project-secret-delegation-not-distribution]].
            match delegate_fetch_and_apply(args.version.as_deref(), &ch_marker, ctx).await {
                Ok(Some(v)) => {
                    applied = Some(v.clone());
                    notes.push(format!("applied v{v} (via delegate-on-miss)"));
                }
                Ok(None) => notes.push("delegate-on-miss: already up to date".into()),
                Err(e) => errors.push(format!("delegate-on-miss failed: {e}")),
            }
        } else if let Some(src) = read_dev_source()
            && args.version.is_none()
        {
            // dev-source branch ignores `args.version` and pulls whatever sha
            // the upstream is currently serving — that's the right semantics
            // when the user just clicks "Apply" with no explicit version
            // (track HEAD), but WRONG when the user picked a tagged build:
            // an explicit `version` is the user saying "leave whatever dev
            // stream this is and go to this tagged release." Fall through
            // to the GitHub-release path in that case. Symmetric with the
            // `dev_gate_skip` exception above. Without this gate, any host
            // ever deployed via `--source` is permanently trapped routing
            // through `apply_update_dev` and can never accept a release.
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
            // Pin policy lives in the version arg itself: selecting a
            // non-channel-latest version pins to it; selecting the channel
            // latest unpins. See [[feedback-one-tool-per-resource]].
            let normalised = normalise_version(ver);
            let latest_tag = match list_versions(&ch_marker, &token).await {
                Ok(v) => v.first().map(|e| e.tag.clone()),
                Err(e) => {
                    errors.push(format!("list versions failed: {e}"));
                    None
                }
            };
            let is_latest = latest_tag.as_deref() == Some(normalised.as_str());
            if is_latest {
                if let Err(e) = clear_version_pin() {
                    errors.push(format!("clear pin failed: {e}"));
                } else if read_version_pin().is_none() {
                    notes.push("pin cleared".into());
                }
            } else if let Err(e) = write_version_pin(&normalised) {
                errors.push(format!("pin failed: {e}"));
            } else {
                notes.push(format!("pinned to {normalised}"));
            }
            match apply_specific_version(&ch_marker, &normalised, &token).await {
                Ok(v) => {
                    applied = Some(v.clone());
                    notes.push(format!("applied v{v}"));
                }
                Err(e) => errors.push(format!("apply v{normalised} failed: {e}")),
            }
        } else {
            // No version arg → update to channel latest. Any existing pin is
            // released (per #6: "If pinned and newer exists → apply unpins
            // and goes to latest").
            match check_for_update(&ch_marker, &token).await {
                Ok(Some(info)) => match apply_update(&info, &token).await {
                    Ok(()) => {
                        if read_version_pin().is_some() {
                            if let Err(e) = clear_version_pin() {
                                errors.push(format!("clear pin failed: {e}"));
                            } else {
                                notes.push("pin cleared".into());
                            }
                        }
                        applied = Some(info.version.clone());
                        notes.push(format!("applied v{}", info.version));
                    }
                    Err(e) => errors.push(format!("apply failed: {e}")),
                },
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
    let update_available = latest
        .as_deref()
        .map(|l| crate::update_state::is_update_available(CURRENT_VERSION, l));

    let pending_restart = crate::update::read_pending_restart()
        .map(|(target, age_secs)| PendingRestart { target, age_secs });

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
        pending_restart,
        update_available,
    })
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Delegate-on-miss: when this peer has no `github_token` secret, ask a
/// paired secure peer that does hold one to fetch the release asset on our
/// behalf. The token never leaves the holder; we get back the verified bytes.
///
/// Returns `Ok(Some(version))` if a peer served the asset and the local
/// binary swap succeeded, `Ok(None)` if no `version` was specified (the
/// caller surfaces a hint), or `Err(_)` when candidate peers existed but
/// every one failed (aggregated reasons in the message).
///
/// This slice requires an explicit `--version`. Channel-latest delegation
/// (asking the holder to resolve the channel's newest tag itself) is a
/// follow-up — see [[project-github-token-auto-provision]].
async fn delegate_fetch_and_apply(
    version: Option<&str>,
    channel: &Channel,
    ctx: &contract::ToolCtx,
) -> Result<Option<String>> {
    let pinned = version
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(normalise_version);
    let target = build_target().to_string();

    let conn = db::open_default().context("open orca.db for peer enumeration")?;
    let candidates: Vec<db::pod::peerdb::PeerRow> = db::pod::peerdb::list_peers(&conn)
        .context("list paired peers")?
        .into_iter()
        .filter(|p| p.departed_at.is_none() && p.peer_secure)
        .collect();
    if candidates.is_empty() {
        anyhow::bail!("no paired secure peers available to delegate fetch");
    }

    // Sanity: surface a clear error if no transport is registered, rather
    // than letting the macro-emitted peer_dispatch fail per-peer.
    ctx.service::<Arc<dyn RemoteExec>>()
        .context("no RemoteExec transport registered for delegate fetch")?;

    let mut errs: Vec<String> = Vec::new();
    for peer in &candidates {
        let args = FetchReleaseAssetArgs {
            version: pinned.clone(),
            target: target.clone(),
            channel: Some(channel.as_marker().to_string()),
        };
        // Setting ctx.peer triggers the macro-emitted peer_dispatch stanza
        // inside `system_serve_release`, routing the call through
        // RemoteExec to `peer.peer_hostname` and returning the typed
        // `FetchReleaseAssetOutput` directly.
        let peered = ctx.clone().with_peer(peer.peer_hostname.clone());
        let out = match system_serve_release(args, &peered).await {
            Ok(o) => o,
            Err(e) => {
                errs.push(format!("{}: {e}", peer.peer_hostname));
                continue;
            }
        };
        let bytes = match utils::encoding::base64_decode(&out.asset_b64) {
            Ok(b) => b,
            Err(e) => {
                errs.push(format!("{}: base64 decode: {e}", peer.peer_hostname));
                continue;
            }
        };
        if let Err(e) = verify_sha256(&bytes, &out.sha256) {
            errs.push(format!("{}: sha256 verify: {e}", peer.peer_hostname));
            continue;
        }
        if let Err(e) = apply_binary(&bytes, &out.version) {
            errs.push(format!("{}: apply_binary: {e}", peer.peer_hostname));
            continue;
        }
        return Ok(Some(out.version));
    }
    anyhow::bail!(
        "all {} delegate peers failed: {}",
        candidates.len(),
        errs.join("; ")
    );
}

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
