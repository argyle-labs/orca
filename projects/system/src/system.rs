//! System install/uninstall lifecycle + system-detail snapshot tool.
//!
//! `system.detail` is the canonical single-call "tell me everything about this
//! host" endpoint: installation paths, orca runtime (version/target/mode/
//! channel/pinned_to), and the full SystemInfoReport (CPU/mem/distro/etc).
//!
//! Slice A4 dissolved the `SystemService` trait — this fn body now calls
//! `install_status::install_status_report()`, `update_state::*`, and
//! `system_info::current_or_collect()` directly. No service indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::system_info_types::SystemInfoReport;

use crate::install_status::install_status_report;
use crate::system_info::current_or_collect;
use crate::update_state::{read_channel_marker, read_version_pin};
use orca_macro::orca_tool;

// ── Shared shapes ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInstalled {
    pub installed: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathLinked {
    pub linked: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathExists {
    pub exists: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PathInitialized {
    pub initialized: bool,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct McpRegistration {
    pub registered: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub struct SystemStatusReport {
    pub binary: PathInstalled,
    pub claude_md: PathLinked,
    pub vault: PathExists,
    pub agents: PathLinked,
    pub pki: PathInitialized,
    pub mcp: McpRegistration,

    // ── Runtime (formerly system.runtime.detail) ────────────────────────────
    /// Orca version from `CARGO_PKG_VERSION` at build time.
    pub version: String,
    /// Build target triple of this binary (e.g. `aarch64-apple-darwin`).
    pub target: String,
    /// "embedded" when this binary was built with the `ui` feature on, "disabled" otherwise.
    pub frontend: String,
    /// Daemon operating mode: "daemon" | "parked" | "dev". `None` when the
    /// state file is absent (binary not running as the registered daemon).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Release channel marker (`stable` | `rc` | `dev`). `None` when no
    /// channel marker has been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Active version pin if any (`orca update --pin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_to: Option<String>,
    /// Cross-platform OS / hardware / process / network snapshot. `None` only
    /// when the collector failed to initialise on this host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfoReport>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SystemStatusArgs {}

/// Snapshot of orca's installation: binary, ~/.claude/CLAUDE.md, vault dir, agents symlink, PKI init, MCP registration.
#[orca_tool(domain = "system", verb = "detail", remote_ok = true)]
async fn system_detail(
    _args: SystemStatusArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SystemStatusReport> {
    let report = install_status_report()?;

    let frontend = if cfg!(feature = "ui") {
        "embedded"
    } else {
        "disabled"
    };
    let version_str = env!("ORCA_VERSION");
    let is_dev_build = version_str.contains("-dev+") || version_str.ends_with("+unknown");
    let mode = if is_dev_build {
        Some("dev".to_string())
    } else {
        orca_utils::state::read()
            .ok()
            .flatten()
            .map(|s| match s.mode {
                orca_utils::state::DaemonMode::Daemon => "daemon".to_string(),
                orca_utils::state::DaemonMode::Parked => "parked".to_string(),
                orca_utils::state::DaemonMode::Dev => "dev".to_string(),
            })
    };
    let channel = read_channel_marker().map(|c| c.as_marker().to_string());
    let pinned_to = read_version_pin();
    let system = Some((*current_or_collect()).clone());

    // `~/.claude/agents` symlink isn't tracked by the typed install report
    // yet; surface it as not-linked with an empty path until the typed
    // reporter learns about it. Matches prior behaviour for hosts that never
    // had the legacy symlink populated.
    let agents_path = report
        .claude_md
        .path
        .parent()
        .map(|p| p.join("agents"))
        .unwrap_or_default();
    let agents_linked = agents_path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    Ok(SystemStatusReport {
        binary: PathInstalled {
            installed: report.binary.installed,
            path: report.binary.path.to_string_lossy().into_owned(),
        },
        claude_md: PathLinked {
            linked: report.claude_md.linked,
            path: report.claude_md.path.to_string_lossy().into_owned(),
        },
        vault: PathExists {
            exists: report.vault.exists,
            path: report.vault.path.to_string_lossy().into_owned(),
        },
        agents: PathLinked {
            linked: agents_linked,
            path: agents_path.to_string_lossy().into_owned(),
        },
        pki: PathInitialized {
            initialized: report.pki.initialized,
            path: report.pki.path.to_string_lossy().into_owned(),
        },
        mcp: McpRegistration {
            registered: report.mcp.registered,
        },
        version: env!("ORCA_VERSION").into(),
        target: env!("ORCA_BUILD_TARGET").into(),
        frontend: frontend.into(),
        mode,
        channel,
        pinned_to,
        system,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_contract::ToolCtx;
    use orca_utils::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn empty_ctx() -> ToolCtx {
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
            db_path: PathBuf::from("/tmp/orca-tools-system-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn system_detail_returns_report() {
        let ctx = empty_ctx();
        // The fn calls real filesystem/env helpers — it must succeed even in
        // hermetic test environments (HOME is set in CI/dev shells).
        let out = system_detail(SystemStatusArgs {}, &ctx).await;
        assert!(out.is_ok(), "system_detail failed: {:?}", out.err());
        let r = out.unwrap();
        assert!(!r.version.is_empty());
        assert!(!r.target.is_empty());
    }
}
