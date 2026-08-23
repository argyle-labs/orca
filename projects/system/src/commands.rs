//! Lifecycle tool surface: install / delete + the unified `system.update` tool
//! that owns every system-update concern (orca binary, channel, pin,
//! dev-source, hostname/fqdn, addressing overrides, OS package upgrade).
//!
//! Per [[feedback-one-tool-per-resource]] there is exactly ONE `system.update`
//! — never a `system.update.apply` / `.pin` / `.unpin` / `host.set` family.

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::capability_tools::CapabilityRow;
use crate::dev::{
    apply_update_dev, check_for_update_dev, clear_dev_source, read_dev_source, write_dev_source,
};
use crate::install::{InstallReport, cmd_install_report, cmd_uninstall_report};
use crate::retention_tools::{RetentionSetArgs, RetentionSetOutput, apply_retention_set};
use crate::sysadmin::{SystemKillOutput, kill_stale};
use crate::update::{
    UpdateInfo, VersionEntry, apply_binary, apply_update, build_target, check_for_update,
    current_binary_path, fetch_release_asset, list_versions, prune_check_cache,
    resolve_github_token, verify_sha256,
};
use crate::update_state::{Channel, read_channel_marker, write_channel_marker};
use contract::RemoteExec;
use derive::orca_tool;
use std::sync::Arc;

const CURRENT_VERSION: &str = env!("ORCA_VERSION");

// ── create{action=install} / delete{action=remove|kill} ────────────────────

/// The `system.create` action. Only `install` today; the enum keeps the
/// six-verb `create{action=…}` shape and room to grow.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum SystemCreateAction {
    /// Install orca on this host.
    #[default]
    Install,
}

/// Args for [`system_create`]. `action` defaults to `install`; the rest do the
/// user-level install. Pass `service_user` (and optional `home_dir` /
/// `admin_pubkey`) to also provision a system service user with SSH access
/// (Linux, root).
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemInstallArgs {
    /// Which create action to run. Defaults to `install`.
    #[serde(default)]
    #[arg(long, value_enum, default_value = "install")]
    pub action: SystemCreateAction,
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

/// [MUTATES STATE] Create/install orca on this host (`action=install`). Always
/// wires the user-level install (binary, ~/.claude symlinks, MCP registration,
/// PKI). When `service_user` is set, also bootstraps a system service user with
/// SSH access — replaces the former separate `system.bootstrap` tool.
/// `local_only`: an install is a host-local action, never peer-dispatchable.
#[orca_tool(domain = "system", verb = "create", local_only = true)]
async fn system_create(args: SystemInstallArgs, _ctx: &contract::ToolCtx) -> Result<InstallReport> {
    let SystemCreateAction::Install = args.action;
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

/// The `system.delete` action.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum SystemDeleteAction {
    /// Uninstall orca from this host (binary, MCP, symlinks, supervisor unit).
    #[default]
    Remove,
    /// Kill stale orca runtime processes so a binary swap is picked up.
    Kill,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemDeleteArgs {
    /// Which delete action to run. Defaults to `remove` (full uninstall).
    #[serde(default)]
    #[arg(long, value_enum, default_value = "remove")]
    pub action: SystemDeleteAction,
}

/// Untagged so each variant serializes as its bare payload.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SystemDeleteOutput {
    Remove(InstallReport),
    Kill(SystemKillOutput),
}

/// [MUTATES STATE] Delete orca state on this host. `action=remove` (default)
/// uninstalls: removes binary, MCP registration, CLAUDE.md symlinks, AND the
/// daemon supervisor unit (launchd / systemd / openrc / unraid) — absorbed the
/// former `system.daemon.uninstall`. `action=kill` reaps stale orca runtime
/// processes (was `system.kill`). `local_only`: both act on host-local
/// processes/state and are never peer-dispatchable.
#[orca_tool(domain = "system", verb = "delete", local_only = true)]
async fn system_delete(
    args: SystemDeleteArgs,
    _ctx: &contract::ToolCtx,
) -> Result<SystemDeleteOutput> {
    match args.action {
        SystemDeleteAction::Remove => {
            let mut report = cmd_uninstall_report();
            match crate::daemon::uninstall_service() {
                Ok(()) => report.done.push("daemon supervisor removed".to_string()),
                Err(e) => report
                    .errors
                    .push(format!("daemon supervisor removal failed: {e}")),
            }
            Ok(SystemDeleteOutput::Remove(report))
        }
        SystemDeleteAction::Kill => Ok(SystemDeleteOutput::Kill(kill_stale())),
    }
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
    // Fast path — serve our own on-disk binary. When the requester wants the
    // exact version + target this peer is already running, we don't need a
    // github_token or GitHub at all: the bytes are already on disk. This makes
    // the mesh self-seeding — once ONE peer of a given arch reaches the target
    // version it can update every same-arch peer, token-holder or not. Only
    // cross-arch or different-version requests fall through to the GitHub path.
    if let Some(req) = args
        .version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && req.trim_start_matches('v') == CURRENT_VERSION
        && args.target == build_target()
    {
        let path = current_binary_path()?;
        let bytes =
            std::fs::read(&path).with_context(|| format!("read own binary {}", path.display()))?;
        let sha256 = utils::hash::sha256_hex(&bytes);
        return Ok(FetchReleaseAssetOutput {
            asset_b64: utils::encoding::base64_encode(&bytes),
            sha256,
            version: CURRENT_VERSION.to_string(),
        });
    }

    // No token needed for the public repo — fetch_release_asset runs
    // unauthenticated when the secret is absent. A token (if present) just
    // raises the rate limit. This peer only needs GitHub egress to serve.
    let token = resolve_github_token();
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
///   - orca binary: `channel`, `version` (one-shot, no pin), `dev_source`, `clear_dev_source`
///   - system identity: `hostname`, `fqdn`
///   - addressing overrides: `lan_v4`, `lan_v6`, `tailscale_v4`, `tailscale_v6`
///   - OS package upgrade: `os_packages`
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SystemUpdateArgs {
    /// Switch update channel: stable | beta. On change, applies latest on the
    /// new channel. (For local hot-reload dev builds use `orca dev enable` —
    /// that is a separate mechanism, not an update channel.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub channel: Option<String>,

    /// Apply a specific version (semver, leading `v` optional) as a one-shot —
    /// it does not pin, and never blocks a later update to latest. Omit to
    /// update to the channel latest. Updates only apply on this explicit
    /// operator action; the daemon never self-applies.
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

    /// Discrete update action. Omit for the default binary/host update flow.
    /// `enable_cap` / `disable_cap` / `recheck_cap` drive the per-host
    /// capability registry (was `system.capability_{enable,disable,recheck}`);
    /// `set_retention` writes retention knobs (was `system.retention_set`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long, value_enum)]
    pub action: Option<SystemUpdateAction>,

    /// Capability provider name for `enable_cap` / `disable_cap` /
    /// `recheck_cap` (e.g. `docker`, `proxmox`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub name: Option<String>,

    /// Operator-visible reason for `disable_cap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    pub reason: Option<String>,

    /// Retention knobs for `action=set_retention`.
    #[command(flatten)]
    #[serde(flatten)]
    pub retention: RetentionSetArgs,
}

/// Discrete `system.update` actions folded in from the retired imperative
/// verbs. `None` (the default) runs the binary/host update flow.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum SystemUpdateAction {
    /// Clear a `Disabled` capability row and immediately re-probe.
    EnableCap,
    /// Mark a capability provider `Disabled` (sticky across restarts).
    DisableCap,
    /// Force a fresh probe of one capability provider.
    RecheckCap,
    /// Set one or more retention knobs; returns the resolved policy.
    SetRetention,
}

/// Untagged so the default (`action` omitted) `Update` variant serializes as a
/// bare `SystemUpdateOutput` — preserving every existing wire decoder (the pod
/// `peer_update_state` cache decodes `SystemUpdateOutput` straight from a `{}`
/// call).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SystemUpdateResult {
    Update(Box<SystemUpdateOutput>),
    Capability(CapabilityRow),
    Retention(RetentionSetOutput),
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
) -> Result<SystemUpdateResult> {
    // Discrete actions dispatch first and short-circuit the binary/host update
    // flow. Each returns its own typed variant.
    match args.action {
        Some(SystemUpdateAction::EnableCap) => {
            let name = require_cap_name(&args)?;
            return Ok(SystemUpdateResult::Capability(
                crate::capability::enable(name).await?.into(),
            ));
        }
        Some(SystemUpdateAction::DisableCap) => {
            let name = require_cap_name(&args)?;
            let reason = args
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`reason` is required for action=disable_cap"))?;
            return Ok(SystemUpdateResult::Capability(
                crate::capability::disable(name, reason)?.into(),
            ));
        }
        Some(SystemUpdateAction::RecheckCap) => {
            let name = require_cap_name(&args)?;
            return Ok(SystemUpdateResult::Capability(
                crate::capability::recheck(name).await?.into(),
            ));
        }
        Some(SystemUpdateAction::SetRetention) => {
            return Ok(SystemUpdateResult::Retention(
                apply_retention_set(&args.retention).await?,
            ));
        }
        None => {}
    }
    Ok(SystemUpdateResult::Update(Box::new(
        run_system_update(args, ctx).await?,
    )))
}

/// Extract the required capability provider name for the `*_cap` actions.
fn require_cap_name(args: &SystemUpdateArgs) -> Result<&str> {
    args.name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("`name` is required for capability actions"))
}

/// The default (`action` omitted) binary/host update flow.
async fn run_system_update(
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
            // The dev update-channel is retired. Local hot-reload builds are
            // driven by `orca dev enable` (cargo-watch), which is independent
            // of the update channel — point the operator there and leave the
            // channel marker on whatever release channel they were tracking.
            notes.push(
                "the `dev` update-channel is retired — use `orca dev enable` for a local \
                 cargo-watch build (that is separate from stable/beta); channel unchanged"
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
    // If the binary is a prerelease but the marker says stable (common on
    // hosts installed without explicit channel selection), treat the host as
    // beta for update-check purposes so we don't compare an rc.9 binary
    // against the latest *stable* release and report a phantom "downgrade
    // available". Only two channels exist now: stable and beta.
    let stored = read_channel_marker().unwrap_or(Channel::Stable);
    let implied = Channel::from_version(CURRENT_VERSION);
    let ch_marker = if stored == Channel::Beta || implied == Channel::Beta {
        Channel::Beta
    } else {
        Channel::Stable
    };
    let token = resolve_github_token();

    // Dev-STATE gate (dev is a state, not a channel — see `update::is_dev`):
    // a dev build tracks a local/HEAD stream, so never pull a GitHub release
    // over it. An explicit `--version` is the operator saying "leave dev, go
    // to this tagged build" — that alone lifts the gate.
    let dev_gate_skip = crate::update::is_dev() && args.version.is_none();
    if binary_intent && !dev_gate_skip {
        if let Some(src) = read_dev_source()
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
            // One-shot apply: no sticky pin. A host always tracks
            // channel-latest; requesting an explicit version applies it once
            // and never blocks a later update to latest.
            let normalised = normalise_version(ver);
            match apply_specific_version(&ch_marker, &normalised, &token).await {
                Ok(v) => {
                    applied = Some(v.clone());
                    notes.push(format!("applied v{v}"));
                }
                // Direct fetch failed. When we have no token (so couldn't reach
                // a private/rate-limited asset, or this host is offline from
                // GitHub) fall back to a paired peer that may hold one.
                Err(e) if token.is_empty() => {
                    match delegate_fetch_and_apply(Some(&normalised), &ch_marker, ctx).await {
                        Ok(Some(v)) => {
                            applied = Some(v.clone());
                            notes.push(format!("applied v{v} (via delegate after direct fail)"));
                        }
                        Ok(None) => notes.push(format!(
                            "apply v{normalised} failed ({e}); delegate: up to date"
                        )),
                        Err(de) => errors.push(format!(
                            "apply v{normalised} failed ({e}); delegate failed: {de}"
                        )),
                    }
                }
                Err(e) => errors.push(format!("apply v{normalised} failed: {e}")),
            }
        } else {
            // No version arg → update to channel latest.
            match check_for_update(&ch_marker, &token).await {
                Ok(Some(info)) => match apply_update(&info, &token).await {
                    Ok(()) => {
                        applied = Some(info.version.clone());
                        notes.push(format!("applied v{}", info.version));
                    }
                    Err(e) if token.is_empty() => {
                        match delegate_fetch_and_apply(None, &ch_marker, ctx).await {
                            Ok(Some(v)) => {
                                applied = Some(v.clone());
                                notes
                                    .push(format!("applied v{v} (via delegate after direct fail)"));
                            }
                            Ok(None) => {
                                notes.push(format!("apply failed ({e}); delegate: up to date"))
                            }
                            Err(de) => {
                                errors.push(format!("apply failed ({e}); delegate failed: {de}"))
                            }
                        }
                    }
                    Err(e) => errors.push(format!("apply failed: {e}")),
                },
                Ok(None) => notes.push(format!("already up to date on {}", ch_marker.as_marker())),
                // Check itself failed (offline / rate-limited). Try a peer.
                Err(e) if token.is_empty() => {
                    match delegate_fetch_and_apply(None, &ch_marker, ctx).await {
                        Ok(Some(v)) => {
                            applied = Some(v.clone());
                            notes.push(format!("applied v{v} (via delegate-on-miss)"));
                        }
                        Ok(None) => notes.push("delegate-on-miss: already up to date".into()),
                        Err(de) => {
                            errors.push(format!("check failed ({e}); delegate failed: {de}"))
                        }
                    }
                }
                Err(e) => errors.push(format!("check failed: {e}")),
            }
        }
    }

    // ── 5. probe current state for the response ───────────────────────────
    // Public repo → list unauthenticated when we have no token (a token just
    // raises the rate limit).
    let available_versions = {
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

    // Report the host's CURRENT identity, not just what THIS call changed. A
    // read-only probe (and a channel-only apply) must still return the live
    // hostname/fqdn so callers can see identity without a separate lookup —
    // previously these were null unless the invocation set them.
    // See [[project-system-update-null-hostname-fqdn]].
    let hostname_out = hostname_applied.or_else(|| {
        db::open_default()
            .ok()
            .and_then(|c| db::settings::get(&c, "host.display_name").ok().flatten())
            .filter(|v| !v.trim().is_empty())
            .or_else(|| Some(crate::host::os_hostname()))
    });
    let fqdn_out = fqdn_applied.or_else(|| {
        db::open_default()
            .ok()
            .and_then(|c| db::settings::get(&c, "host.fqdn").ok().flatten())
            .filter(|v| !v.trim().is_empty())
    });

    Ok(SystemUpdateOutput {
        current_version: CURRENT_VERSION.to_string(),
        channel: ch_marker.as_marker().to_string(),
        // Pin removed: hosts always track channel-latest. Field kept for wire
        // compatibility with older peers; always None now.
        pinned_to: None,
        dev_source: read_dev_source(),
        available_versions,
        latest,
        applied,
        hostname: hostname_out,
        fqdn: fqdn_out,
        addressing_set,
        os_package_result,
        notes,
        errors,
        pending_restart,
        update_available,
    })
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Build an actionable error when no paired *secure* peer exists to delegate a
/// private-asset fetch to. Because the caller only reaches this after filtering
/// out every secure peer, `insecure_present` is exactly the set of present
/// (non-departed) peers — the candidates the operator could trust. When it is
/// empty the host has no paired peers at all, which we call out distinctly.
pub(crate) fn no_secure_peer_message(insecure_present: &[(String, String)]) -> String {
    if insecure_present.is_empty() {
        return "no paired secure peer available to delegate the private-asset fetch, and this \
                host has no paired peers at all. Pair a peer that holds a `github_token` and \
                trust it with `pod trust <peer_id> --on true --push`, or set a local \
                `github_token` secret to fetch the asset directly."
            .to_string();
    }
    let candidates = insecure_present
        .iter()
        .map(|(id, host)| format!("{host} ({id})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no paired secure peer available to delegate the private-asset fetch. Trust a candidate \
         peer with `pod trust <peer_id> --on true --push` (candidates: {candidates}), or set a \
         local `github_token` secret to fetch the asset directly."
    )
}

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
    let present: Vec<db::pod::peerdb::PeerRow> = db::pod::peerdb::list_peers(&conn)
        .context("list paired peers")?
        .into_iter()
        .filter(|p| p.departed_at.is_none())
        .collect();
    let candidates: Vec<&db::pod::peerdb::PeerRow> =
        present.iter().filter(|p| p.peer_secure).collect();
    if candidates.is_empty() {
        let insecure: Vec<(String, String)> = present
            .iter()
            .filter(|p| !p.peer_secure)
            .map(|p| (p.peer_id.clone(), p.peer_hostname.clone()))
            .collect();
        anyhow::bail!(no_secure_peer_message(&insecure));
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
    // Public repo: unauthenticated tag lookup works; token optional.
    let url = format!("{APP_REPO_API_URL}/releases/tags/{v_tag}");
    let client = utils::http::Client::new();
    let user_agent = format!("{APP_NAME}/{CURRENT_VERSION}");
    let req = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", &user_agent);
    let req = if token.is_empty() {
        req
    } else {
        req.bearer(token)
    };
    let resp = req.send().await.context("fetch release by tag")?;
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

/// Non-blocking startup update check — prints a notice, never downloads or
/// applies. Updates are only applied on an explicit operator `system update`.
pub async fn startup_update_check() {
    // Token optional — public-repo checks run unauthenticated.
    let token = resolve_github_token();
    let channel = read_channel_marker().unwrap_or(Channel::Stable);
    if let Ok(Some(info)) = check_for_update(&channel, &token).await {
        println!(
            "[orca] update available: v{} on '{}' — run `orca system update` to upgrade",
            info.version,
            channel.as_marker()
        );
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
    fn no_secure_peer_message_lists_candidates_and_remedy() {
        let msg = no_secure_peer_message(&[
            ("id-1".to_string(), "alpha".to_string()),
            ("id-2".to_string(), "beta".to_string()),
        ]);
        assert!(msg.contains("pod trust <peer_id> --on true --push"));
        assert!(msg.contains("alpha (id-1)"));
        assert!(msg.contains("beta (id-2)"));
        assert!(msg.contains("github_token"));
    }

    #[test]
    fn no_secure_peer_message_calls_out_zero_peers() {
        let msg = no_secure_peer_message(&[]);
        assert!(msg.contains("no paired peers at all"));
        assert!(msg.contains("pod trust <peer_id> --on true --push"));
        assert!(msg.contains("github_token"));
    }

    #[test]
    fn system_update_output_decodes_empty_object() {
        let decoded: SystemUpdateOutput = serde_json::from_str("{}").unwrap();
        assert!(decoded.applied.is_none());
        assert!(decoded.errors.is_empty());
    }

    // ── require_cap_name ────────────────────────────────────────────────────

    #[test]
    fn require_cap_name_returns_trimmed_name() {
        let args = SystemUpdateArgs {
            name: Some("  docker  ".to_string()),
            ..Default::default()
        };
        assert_eq!(require_cap_name(&args).unwrap(), "docker");
    }

    #[test]
    fn require_cap_name_errors_when_missing() {
        let args = SystemUpdateArgs::default();
        let err = require_cap_name(&args).unwrap_err().to_string();
        assert!(err.contains("`name` is required"), "{err}");
    }

    #[test]
    fn require_cap_name_errors_when_blank() {
        let args = SystemUpdateArgs {
            name: Some("   ".to_string()),
            ..Default::default()
        };
        assert!(require_cap_name(&args).is_err());
    }

    // ── which ───────────────────────────────────────────────────────────────

    #[test]
    fn which_finds_sh() {
        assert!(which("sh"), "`sh` must resolve on any POSIX host");
    }

    #[test]
    fn which_rejects_nonexistent_command() {
        assert!(!which("orca-definitely-not-a-real-binary-xyzzy"));
    }

    // ── normalise_version edge cases ─────────────────────────────────────────

    #[test]
    fn normalise_version_leaves_bare_v() {
        // A stray leading `v` is preserved verbatim (no double-prefix).
        assert_eq!(normalise_version("v"), "v");
        assert_eq!(normalise_version("version1"), "version1");
    }

    // ── set_os_hostname validation (error branch, no process spawn) ──────────

    #[tokio::test]
    async fn set_os_hostname_rejects_empty() {
        let err = set_os_hostname("").await.unwrap_err().to_string();
        assert!(err.contains("invalid hostname"), "{err}");
    }

    #[tokio::test]
    async fn set_os_hostname_rejects_whitespace() {
        assert!(set_os_hostname("bad name").await.is_err());
        assert!(set_os_hostname("tab\tname").await.is_err());
    }

    // ── enum defaults ────────────────────────────────────────────────────────

    #[test]
    fn create_action_defaults_to_install() {
        assert_eq!(SystemCreateAction::default(), SystemCreateAction::Install);
    }

    #[test]
    fn delete_action_defaults_to_remove() {
        assert_eq!(SystemDeleteAction::default(), SystemDeleteAction::Remove);
    }

    // ── enum serde (snake_case value_enum) ───────────────────────────────────

    #[test]
    fn update_action_serializes_snake_case() {
        let cases = [
            (SystemUpdateAction::EnableCap, "\"enable_cap\""),
            (SystemUpdateAction::DisableCap, "\"disable_cap\""),
            (SystemUpdateAction::RecheckCap, "\"recheck_cap\""),
            (SystemUpdateAction::SetRetention, "\"set_retention\""),
        ];
        for (variant, wire) in cases {
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
            let back: SystemUpdateAction = serde_json::from_str(wire).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn delete_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SystemDeleteAction::Kill).unwrap(),
            "\"kill\""
        );
        let back: SystemDeleteAction = serde_json::from_str("\"remove\"").unwrap();
        assert_eq!(back, SystemDeleteAction::Remove);
    }

    // ── untagged output shaping ──────────────────────────────────────────────

    #[test]
    fn update_result_update_variant_serializes_bare() {
        // Untagged: the default Update variant must serialize as a bare
        // SystemUpdateOutput (top-level `current_version`), preserving the
        // wire contract older peers decode with.
        let out = SystemUpdateOutput {
            current_version: "0.0.9".to_string(),
            channel: "beta".to_string(),
            ..Default::default()
        };
        let result = SystemUpdateResult::Update(Box::new(out));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"current_version\":\"0.0.9\""), "{json}");
        assert!(json.contains("\"channel\":\"beta\""), "{json}");
        // No enum tag wrapper on the untagged variant.
        assert!(!json.contains("\"Update\""), "{json}");
    }

    #[test]
    fn delete_output_kill_serializes_as_bare_payload() {
        let out = SystemDeleteOutput::Kill(SystemKillOutput {
            killed_patterns: vec!["mcp-serve".to_string()],
        });
        let json = serde_json::to_string(&out).unwrap();
        // Untagged: no `Kill` wrapper key — the payload is bare.
        assert!(!json.contains("\"Kill\""), "{json}");
        assert!(
            json.contains("\"killed_patterns\":[\"mcp-serve\"]"),
            "{json}"
        );
    }

    // ── PendingRestart / FetchReleaseAssetOutput serde ───────────────────────

    #[test]
    fn pending_restart_round_trips() {
        let pr = PendingRestart {
            target: "0.0.9".to_string(),
            age_secs: 42,
        };
        let json = serde_json::to_string(&pr).unwrap();
        let back: PendingRestart = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target, "0.0.9");
        assert_eq!(back.age_secs, 42);
        // #[serde(default)] tolerates a partial payload from an older peer.
        let partial: PendingRestart = serde_json::from_str(r#"{"target":"x"}"#).unwrap();
        assert_eq!(partial.age_secs, 0);
    }

    #[test]
    fn fetch_release_asset_output_defaults_empty() {
        let out = FetchReleaseAssetOutput::default();
        assert!(out.asset_b64.is_empty());
        assert!(out.sha256.is_empty());
        assert!(out.version.is_empty());
    }

    // ── SystemUpdateArgs serde defaults ──────────────────────────────────────

    #[test]
    fn update_args_empty_object_is_readonly_probe() {
        let args: SystemUpdateArgs = serde_json::from_str("{}").unwrap();
        assert!(args.channel.is_none());
        assert!(args.version.is_none());
        assert!(!args.clear_dev_source);
        assert!(!args.os_packages);
        assert!(!args.refresh_host);
        assert!(args.action.is_none());
    }

    #[test]
    fn update_args_parses_action_and_name() {
        let args: SystemUpdateArgs =
            serde_json::from_str(r#"{"action":"disable_cap","name":"docker","reason":"maint"}"#)
                .unwrap();
        assert_eq!(args.action, Some(SystemUpdateAction::DisableCap));
        assert_eq!(require_cap_name(&args).unwrap(), "docker");
        assert_eq!(args.reason.as_deref(), Some("maint"));
    }

    // ── no_secure_peer_message single candidate ──────────────────────────────

    #[test]
    fn no_secure_peer_message_single_candidate() {
        let msg = no_secure_peer_message(&[("id-9".to_string(), "gamma".to_string())]);
        assert!(msg.contains("gamma (id-9)"), "{msg}");
        // With at least one candidate it does NOT claim there are no peers.
        assert!(!msg.contains("no paired peers at all"), "{msg}");
        assert!(
            msg.contains("pod trust <peer_id> --on true --push"),
            "{msg}"
        );
    }

    // ── require_cap_name: reason present but name absent still errors ─────────

    #[test]
    fn require_cap_name_errors_when_only_reason_present() {
        let args = SystemUpdateArgs {
            reason: Some("maintenance".to_string()),
            ..Default::default()
        };
        assert!(require_cap_name(&args).is_err());
    }

    // ── normalise_version: empty input ───────────────────────────────────────

    #[test]
    fn normalise_version_empty_gets_v_prefix() {
        assert_eq!(normalise_version(""), "v");
    }

    // ── SystemCreateAction / SystemDeleteAction serde (snake_case) ───────────

    #[test]
    fn create_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SystemCreateAction::Install).unwrap(),
            "\"install\""
        );
        let back: SystemCreateAction = serde_json::from_str("\"install\"").unwrap();
        assert_eq!(back, SystemCreateAction::Install);
    }

    #[test]
    fn delete_action_remove_round_trips() {
        assert_eq!(
            serde_json::to_string(&SystemDeleteAction::Remove).unwrap(),
            "\"remove\""
        );
        let back: SystemDeleteAction = serde_json::from_str("\"kill\"").unwrap();
        assert_eq!(back, SystemDeleteAction::Kill);
    }

    // ── install / delete args serde defaults & parsing ───────────────────────

    #[test]
    fn install_args_empty_object_defaults_to_install() {
        let args: SystemInstallArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(args.action, SystemCreateAction::Install);
        assert!(args.service_user.is_none());
        assert!(args.home_dir.is_none());
        assert!(args.admin_pubkey.is_none());
        assert!(args.port.is_none());
    }

    #[test]
    fn install_args_parses_service_user_bundle() {
        let args: SystemInstallArgs = serde_json::from_str(
            r#"{"service_user":"orca","home_dir":"/var/lib/orca","admin_pubkey":"ssh-ed25519 AAAA","port":8099}"#,
        )
        .unwrap();
        assert_eq!(args.service_user.as_deref(), Some("orca"));
        assert_eq!(args.home_dir.as_deref(), Some("/var/lib/orca"));
        assert_eq!(args.admin_pubkey.as_deref(), Some("ssh-ed25519 AAAA"));
        assert_eq!(args.port, Some(8099));
    }

    #[test]
    fn install_args_omits_none_fields_on_serialize() {
        let args = SystemInstallArgs::default();
        let json = serde_json::to_string(&args).unwrap();
        // skip_serializing_if = Option::is_none — only `action` survives.
        assert!(json.contains("\"action\":\"install\""), "{json}");
        assert!(!json.contains("service_user"), "{json}");
        assert!(!json.contains("port"), "{json}");
    }

    #[test]
    fn delete_args_empty_object_defaults_to_remove() {
        let args: SystemDeleteArgs = serde_json::from_str("{}").unwrap();
        assert_eq!(args.action, SystemDeleteAction::Remove);
    }

    #[test]
    fn delete_args_parses_kill() {
        let args: SystemDeleteArgs = serde_json::from_str(r#"{"action":"kill"}"#).unwrap();
        assert_eq!(args.action, SystemDeleteAction::Kill);
    }

    // ── SystemDeleteOutput::Remove untagged shaping ──────────────────────────

    #[test]
    fn delete_output_remove_serializes_as_bare_report() {
        let out = SystemDeleteOutput::Remove(InstallReport {
            done: vec!["daemon supervisor removed".to_string()],
            skipped: vec![],
            errors: vec![],
        });
        let json = serde_json::to_string(&out).unwrap();
        // Untagged: no `Remove` wrapper; InstallReport fields are bare.
        assert!(!json.contains("\"Remove\""), "{json}");
        assert!(
            json.contains("\"done\":[\"daemon supervisor removed\"]"),
            "{json}"
        );
        assert!(json.contains("\"skipped\":[]"), "{json}");
        assert!(json.contains("\"errors\":[]"), "{json}");
    }

    // ── SystemUpdateResult::Capability untagged shaping ──────────────────────

    #[test]
    fn update_result_capability_serializes_bare_camel_case() {
        let row = CapabilityRow {
            provider: "docker".to_string(),
            state: "disabled".to_string(),
            last_probed: 1_700_000_000,
            reason: Some("maintenance".to_string()),
            detail: None,
        };
        let result = SystemUpdateResult::Capability(row);
        let json = serde_json::to_string(&result).unwrap();
        // Untagged: no `Capability` wrapper; camelCase field names.
        assert!(!json.contains("\"Capability\""), "{json}");
        assert!(json.contains("\"provider\":\"docker\""), "{json}");
        assert!(json.contains("\"state\":\"disabled\""), "{json}");
        assert!(json.contains("\"lastProbed\":1700000000"), "{json}");
        assert!(json.contains("\"reason\":\"maintenance\""), "{json}");
    }

    // ── FetchReleaseAssetArgs / Output serde ─────────────────────────────────

    #[test]
    fn fetch_release_args_parses_all_fields() {
        let args: FetchReleaseAssetArgs = serde_json::from_str(
            r#"{"version":"v0.0.6-rc.15","target":"x86_64-unknown-linux-gnu","channel":"beta"}"#,
        )
        .unwrap();
        assert_eq!(args.version.as_deref(), Some("v0.0.6-rc.15"));
        assert_eq!(args.target, "x86_64-unknown-linux-gnu");
        assert_eq!(args.channel.as_deref(), Some("beta"));
    }

    #[test]
    fn fetch_release_args_defaults_version_and_channel_none() {
        let args: FetchReleaseAssetArgs =
            serde_json::from_str(r#"{"target":"aarch64-apple-darwin"}"#).unwrap();
        assert!(args.version.is_none());
        assert!(args.channel.is_none());
        assert_eq!(args.target, "aarch64-apple-darwin");
    }

    #[test]
    fn fetch_release_output_round_trips_with_values() {
        let out = FetchReleaseAssetOutput {
            asset_b64: "QUJD".to_string(),
            sha256: "deadbeef".to_string(),
            version: "0.0.9".to_string(),
        };
        let json = serde_json::to_string(&out).unwrap();
        let back: FetchReleaseAssetOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back.asset_b64, "QUJD");
        assert_eq!(back.sha256, "deadbeef");
        assert_eq!(back.version, "0.0.9");
    }

    // ── SystemUpdateOutput full serialize shaping ────────────────────────────

    #[test]
    fn update_output_serializes_all_populated_fields() {
        let out = SystemUpdateOutput {
            current_version: "0.0.8".to_string(),
            channel: "stable".to_string(),
            addressing_set: vec!["lan_v4=10.0.0.2".to_string()],
            hostname: Some("host-a".to_string()),
            fqdn: Some("host-a.example".to_string()),
            os_package_result: Some("upgraded".to_string()),
            update_available: Some(true),
            notes: vec!["applied v0.0.8".to_string()],
            ..Default::default()
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("\"addressing_set\":[\"lan_v4=10.0.0.2\"]"),
            "{json}"
        );
        assert!(json.contains("\"hostname\":\"host-a\""), "{json}");
        assert!(json.contains("\"fqdn\":\"host-a.example\""), "{json}");
        assert!(
            json.contains("\"os_package_result\":\"upgraded\""),
            "{json}"
        );
        assert!(json.contains("\"update_available\":true"), "{json}");
        assert!(json.contains("\"pinned_to\":null"), "{json}");
    }

    #[test]
    fn update_output_default_is_all_empty() {
        let out = SystemUpdateOutput::default();
        assert!(out.current_version.is_empty());
        assert!(out.channel.is_empty());
        assert!(out.pinned_to.is_none());
        assert!(out.dev_source.is_none());
        assert!(out.available_versions.is_empty());
        assert!(out.latest.is_none());
        assert!(out.applied.is_none());
        assert!(out.hostname.is_none());
        assert!(out.fqdn.is_none());
        assert!(out.addressing_set.is_empty());
        assert!(out.os_package_result.is_none());
        assert!(out.notes.is_empty());
        assert!(out.errors.is_empty());
        assert!(out.pending_restart.is_none());
        assert!(out.update_available.is_none());
    }

    // ── SystemUpdateArgs parsing across surfaces ─────────────────────────────

    #[test]
    fn update_args_parses_identity_and_addressing() {
        let args: SystemUpdateArgs = serde_json::from_str(
            r#"{"hostname":"maple","fqdn":"maple.lan","lan_v4":"10.0.0.5","lan_v6":"fe80::1","tailscale_v4":"100.64.0.1","tailscale_v6":"fd7a::1"}"#,
        )
        .unwrap();
        assert_eq!(args.hostname.as_deref(), Some("maple"));
        assert_eq!(args.fqdn.as_deref(), Some("maple.lan"));
        assert_eq!(args.lan_v4.as_deref(), Some("10.0.0.5"));
        assert_eq!(args.lan_v6.as_deref(), Some("fe80::1"));
        assert_eq!(args.tailscale_v4.as_deref(), Some("100.64.0.1"));
        assert_eq!(args.tailscale_v6.as_deref(), Some("fd7a::1"));
    }

    #[test]
    fn update_args_parses_channel_version_devsource_daemon() {
        let args: SystemUpdateArgs = serde_json::from_str(
            r#"{"channel":"beta","version":"0.0.9","dev_source":"http://x/","clear_dev_source":true,"daemon":"park","os_packages":true,"refresh_host":true}"#,
        )
        .unwrap();
        assert_eq!(args.channel.as_deref(), Some("beta"));
        assert_eq!(args.version.as_deref(), Some("0.0.9"));
        assert_eq!(args.dev_source.as_deref(), Some("http://x/"));
        assert!(args.clear_dev_source);
        assert_eq!(args.daemon.as_deref(), Some("park"));
        assert!(args.os_packages);
        assert!(args.refresh_host);
    }

    #[test]
    fn update_args_flattens_retention_knobs() {
        let args: SystemUpdateArgs = serde_json::from_str(
            r#"{"action":"set_retention","days":30,"maxMb":512,"maxRows":10000,"peer":"peer-1"}"#,
        )
        .unwrap();
        assert_eq!(args.action, Some(SystemUpdateAction::SetRetention));
        assert_eq!(args.retention.days, Some(30.0));
        assert_eq!(args.retention.max_mb, Some(512.0));
        assert_eq!(args.retention.max_rows, Some(10000));
        assert_eq!(args.retention.peer.as_deref(), Some("peer-1"));
    }

    #[test]
    fn update_args_parses_enable_cap() {
        let args: SystemUpdateArgs =
            serde_json::from_str(r#"{"action":"enable_cap","name":"proxmox"}"#).unwrap();
        assert_eq!(args.action, Some(SystemUpdateAction::EnableCap));
        assert_eq!(require_cap_name(&args).unwrap(), "proxmox");
    }

    // ── PendingRestart clone / debug ─────────────────────────────────────────

    #[test]
    fn pending_restart_clone_and_debug() {
        let pr = PendingRestart {
            target: "0.0.9".to_string(),
            age_secs: 7,
        };
        let cloned = pr.clone();
        assert_eq!(cloned.target, pr.target);
        assert_eq!(cloned.age_secs, pr.age_secs);
        assert!(format!("{pr:?}").contains("0.0.9"));
    }
}
