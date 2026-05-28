//! `system.update.*` tool surface. Each verb is a `#[orca_tool]` so the macro
//! emits CLI/REST/MCP/WASM uniformly. The implementation helpers live in
//! `crate::update`, `crate::dev`, and `crate::update_state`.

use crate::dev::{
    apply_update_dev, check_for_update_dev, clear_dev_source, read_dev_source, write_dev_source,
};
use crate::update::{
    apply_update, check_for_update, download_asset, prune_check_cache, resolve_github_token,
    write_cached_sha256,
};
use crate::update_state::{
    Channel, clear_version_pin, read_channel_marker, resolve_channel, resolve_pin_veto,
    write_channel_marker, write_version_pin,
};
use anyhow::Result;
use orca_contract::ToolCtx;
use orca_macro::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = match option_env!("ORCA_BUILD_TARGET") {
    Some(v) => v,
    None => "unknown-target",
};

// ── system.update.apply ──────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateApplyArgs {
    /// Channel override: stable | rc | beta | alpha. Falls back to channel marker.
    #[cfg_attr(feature = "cli", arg(long, default_value = ""))]
    #[serde(default)]
    pub channel: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdateApplyOutput {
    pub current_version: String,
    pub applied_version: Option<String>,
    pub channel: String,
    pub note: String,
}

/// Apply the latest update on the configured channel. Reads `~/.orca/channel`
/// when no channel given; rewrites it on success. Uses dev-source when set.
#[orca_tool(domain = "system.update", verb = "apply")]
async fn update_apply(args: UpdateApplyArgs, _ctx: &ToolCtx) -> Result<UpdateApplyOutput> {
    let channel_arg = args.channel.trim();
    if channel_arg.is_empty()
        && let Some(src) = read_dev_source()
    {
        println!("[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel=dev)");
        println!("[orca] checking dev source {src}...");
        let applied = match check_for_update_dev(&src).await? {
            None => {
                println!("[orca] already up to date");
                None
            }
            Some(v) => {
                apply_update_dev(&src).await?;
                Some(v)
            }
        };
        return Ok(UpdateApplyOutput {
            current_version: CURRENT_VERSION.to_string(),
            applied_version: applied,
            channel: "dev".to_string(),
            note: format!("dev-source: {src}"),
        });
    }

    let channel = resolve_channel(channel_arg);
    let token = resolve_github_token();
    println!(
        "[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel={})",
        channel.as_marker()
    );
    println!("[orca] checking for updates...");

    let mut applied = None;
    let mut note = String::new();
    match check_for_update(&channel, &token).await? {
        None => {
            println!("[orca] already up to date");
            note = "already up to date".to_string();
        }
        Some(info) => {
            if let Some(pin) = resolve_pin_veto(&info.version) {
                println!(
                    "[orca] pinned to {pin}; available v{} — run `orca update --unpin` to upgrade",
                    info.version
                );
                note = format!("pinned to {pin}; available v{}", info.version);
            } else {
                println!("[orca] new version available: v{}", info.version);
                apply_update(&info, &token).await?;
                applied = Some(info.version);
            }
        }
    }

    if let Err(e) = write_channel_marker(&channel) {
        eprintln!("[orca] warning: could not update channel marker: {e}");
    }

    Ok(UpdateApplyOutput {
        current_version: CURRENT_VERSION.to_string(),
        applied_version: applied,
        channel: channel.as_marker().to_string(),
        note,
    })
}

// ── system.update.check ──────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateCheckArgs {
    #[cfg_attr(feature = "cli", arg(long, default_value = ""))]
    #[serde(default)]
    pub channel: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdateCheckOutput {
    pub current_version: String,
    pub available_version: Option<String>,
    pub pinned_to: Option<String>,
    pub channel: String,
}

/// Preview only — resolve the target version on the channel and cache its sha256.
/// Does NOT replace the running binary.
#[orca_tool(domain = "system.update", verb = "check")]
async fn update_check(args: UpdateCheckArgs, _ctx: &ToolCtx) -> Result<UpdateCheckOutput> {
    prune_check_cache();
    let channel = resolve_channel(args.channel.trim());
    let token = resolve_github_token();
    println!(
        "[orca] current version: v{CURRENT_VERSION} ({BUILD_TARGET}, channel={})",
        channel.as_marker()
    );
    println!("[orca] checking for updates (preview only)...");

    let mut available = None;
    let mut pinned = None;
    match check_for_update(&channel, &token).await? {
        None => println!("[orca] already up to date"),
        Some(info) => {
            println!("[orca] new version available: v{}", info.version);
            available = Some(info.version.clone());
            if let Some(pin) = resolve_pin_veto(&info.version) {
                println!("[orca] pinned to {pin} — `orca update --unpin` to upgrade");
                pinned = Some(pin);
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
    Ok(UpdateCheckOutput {
        current_version: CURRENT_VERSION.to_string(),
        available_version: available,
        pinned_to: pinned,
        channel: channel.as_marker().to_string(),
    })
}

// ── system.update.pin / unpin ────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdatePinArgs {
    /// Version to pin (e.g. `0.0.4-rc.1` or `v0.0.4-rc.1`).
    pub version: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdatePinOutput {
    pub pinned_to: String,
}

/// Pin to a version. Future `system.update.apply` runs will not upgrade past this.
#[orca_tool(domain = "system.update", verb = "pin")]
async fn update_pin(args: UpdatePinArgs, _ctx: &ToolCtx) -> Result<UpdatePinOutput> {
    let version = args.version.trim();
    if version.is_empty() {
        anyhow::bail!("version must not be empty");
    }
    let normalised = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    write_version_pin(&normalised)?;
    println!("[orca] pinned to {normalised}");
    Ok(UpdatePinOutput {
        pinned_to: normalised,
    })
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateUnpinArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdateUnpinOutput {
    pub cleared: bool,
}

/// Clear the version pin. `system.update.apply` resumes following the channel.
#[orca_tool(domain = "system.update", verb = "unpin")]
async fn update_unpin(_args: UpdateUnpinArgs, _ctx: &ToolCtx) -> Result<UpdateUnpinOutput> {
    clear_version_pin()?;
    println!("[orca] pin cleared");
    Ok(UpdateUnpinOutput { cleared: true })
}

// ── system.update.set-source / clear-source ──────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateSetSourceArgs {
    /// Dev-source URL (e.g. http://10.10.10.40:12009).
    pub url: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdateSetSourceOutput {
    pub url: String,
}

/// Set a dev-source URL. Future `system.update.apply` runs pull from there instead of GitHub.
#[orca_tool(domain = "system.update", verb = "set-source")]
async fn update_set_source(
    args: UpdateSetSourceArgs,
    _ctx: &ToolCtx,
) -> Result<UpdateSetSourceOutput> {
    let url = args.url.trim();
    if url.is_empty() {
        anyhow::bail!("URL must not be empty");
    }
    write_dev_source(url)?;
    println!("[orca] dev source set to {url}");
    Ok(UpdateSetSourceOutput {
        url: url.to_string(),
    })
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UpdateClearSourceArgs {}

#[derive(Serialize, Deserialize, JsonSchema, Debug)]
pub struct UpdateClearSourceOutput {
    pub cleared: bool,
}

/// Clear the dev-source URL, reverting to GitHub-based updates.
#[orca_tool(domain = "system.update", verb = "clear-source")]
async fn update_clear_source(
    _args: UpdateClearSourceArgs,
    _ctx: &ToolCtx,
) -> Result<UpdateClearSourceOutput> {
    clear_dev_source()?;
    println!("[orca] dev source cleared — `system.update.apply` will use GitHub again");
    Ok(UpdateClearSourceOutput { cleared: true })
}

// ── startup notice (called by serve loop) ────────────────────────────────────

/// Non-blocking startup update check — prints a notice, does not download.
/// Channel comes from the install marker, falling back to Stable.
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
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use crate::update_state::read_version_pin;
    use orca_utils::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    fn isolated_orca_home(scenario: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("ORCA_HOME", dir.path());
            std::env::set_var("ORCA_TEST_SCENARIO", scenario);
        }
        dir
    }

    fn marker_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn ctx() -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-update-cmd-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn update_pin_normalises_version() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_cmd");
        let out = update_pin(
            UpdatePinArgs {
                version: "0.0.4-rc.1".to_string(),
            },
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(out.pinned_to, "v0.0.4-rc.1");
        assert_eq!(read_version_pin(), Some("v0.0.4-rc.1".to_string()));
    }

    #[tokio::test]
    async fn update_pin_preserves_v_prefix() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("pin_cmd_v");
        let out = update_pin(
            UpdatePinArgs {
                version: "v0.0.4-rc.1".to_string(),
            },
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(out.pinned_to, "v0.0.4-rc.1");
    }

    #[tokio::test]
    async fn update_set_source_empty_returns_err() {
        let err = update_set_source(UpdateSetSourceArgs { url: String::new() }, &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn update_set_then_clear_source() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("set_source");
        update_set_source(
            UpdateSetSourceArgs {
                url: "http://localhost:8080".to_string(),
            },
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(read_dev_source(), Some("http://localhost:8080".to_string()));
        update_clear_source(UpdateClearSourceArgs {}, &ctx())
            .await
            .unwrap();
        assert!(read_dev_source().is_none());
    }

    #[tokio::test]
    async fn update_unpin_no_pin() {
        let _g = marker_lock();
        let _dir = isolated_orca_home("unpin_noop");
        update_unpin(UpdateUnpinArgs {}, &ctx()).await.unwrap();
    }

    #[tokio::test]
    async fn update_pin_empty_returns_err() {
        let err = update_pin(
            UpdatePinArgs {
                version: String::new(),
            },
            &ctx(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
