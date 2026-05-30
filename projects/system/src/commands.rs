//! Lifecycle tool surface: install / update / uninstall + project listing.
//!
//! Slice B3 dissolved the `LifecycleService` trait — every tool body here
//! calls `crate::install::*`, `crate::update::*`, `crate::update_state::*`,
//! and `crate::dev::*` directly. No service indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::install::InstallReport;

use crate::dev::cmd_dev_enable;
use crate::install::{cmd_install_report, cmd_uninstall_report};
use crate::update::{apply_update, check_for_update, resolve_github_token};
use crate::update_state::{
    clear_version_pin, read_channel_marker, resolve_channel, resolve_pin_veto,
    write_channel_marker, write_version_pin,
};
use derive::orca_tool;

// ── Args ────────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SystemUpdateArgs {
    /// Version or channel to switch to, then apply.
    /// Channels: "stable" | "rc" | "dev".
    /// Pinned version: "0.0.4-rc.11" (leading "v" optional).
    /// "dev" tracks GitHub HEAD via cargo-watch; others pull release binaries.
    /// Omit to apply the latest on the current channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long))]
    pub version: Option<String>,
    /// When set, proxy the call to the named remote peer via the pod mesh
    /// instead of running on the local host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "cli", arg(long, hide = true))]
    pub peer_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ProjectsListReport {
    pub projects: Vec<String>,
}

// ── Tools ───────────────────────────────────────────────────────────────────

/// [MUTATES STATE] Install orca on this host: wire symlinks, register MCP server, install binary.
/// `local_only`: bootstrap-style op that wires local filesystem; not meaningful via pod/exec.
#[orca_tool(domain = "system", verb = "create", local_only = true)]
async fn system_create(
    _args: EmptyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<InstallReport> {
    Ok(cmd_install_report())
}

/// [MUTATES STATE] Uninstall orca from this host: remove binary, MCP registration, and CLAUDE.md symlinks.
/// `local_only`: tears down local filesystem; not meaningful via pod/exec.
#[orca_tool(domain = "system", verb = "delete", local_only = true)]
async fn system_delete(
    _args: EmptyArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<InstallReport> {
    Ok(cmd_uninstall_report())
}

/// [MUTATES STATE] Update orca on this host.
/// Optionally pass `version` to switch channel or pin before applying:
/// "stable" | "rc" | "dev" | "<semver>". "dev" tracks GitHub HEAD via
/// cargo-watch. Omit to apply the latest on the current channel.
/// When `peer_id` is set the update runs on the named peer instead of locally.
#[orca_tool(domain = "system", verb = "update", peer_dispatch = true)]
async fn system_update(
    args: SystemUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<InstallReport> {
    if let Some(ref v) = args.version {
        match v.as_str() {
            "dev" => {
                tokio::task::spawn_blocking(cmd_dev_enable).await??;
            }
            "stable" | "rc" => {
                let ch = resolve_channel(v);
                write_channel_marker(&ch)?;
                _ = clear_version_pin();
            }
            other => {
                let trimmed = other.trim();
                anyhow::ensure!(!trimmed.is_empty(), "version must not be empty");
                let normalised = if trimmed.starts_with('v') {
                    trimmed.to_string()
                } else {
                    format!("v{trimmed}")
                };
                write_version_pin(&normalised)?;
            }
        }
    }

    let ch = read_channel_marker().unwrap_or_else(|| resolve_channel("stable"));
    let resolved = ch.as_marker();
    let mut report = InstallReport {
        done: vec![],
        skipped: vec![],
        errors: vec![],
    };
    let token = resolve_github_token();
    if token.is_empty() {
        report
            .errors
            .push("no github token — set secret 'github_token' or export GITHUB_TOKEN".into());
        return Ok(report);
    }
    match check_for_update(&ch, &token).await? {
        None => report
            .skipped
            .push(format!("already up to date on '{resolved}'")),
        Some(info) => {
            if let Some(pin) = resolve_pin_veto(&info.version) {
                report.skipped.push(format!(
                    "pinned to {pin}; available v{} — run `orca system update --version stable` to unpin",
                    info.version
                ));
            } else {
                match apply_update(&info, &token).await {
                    Ok(()) => report.done.push(format!("updated to {}", info.version)),
                    Err(e) => report.errors.push(format!("apply failed: {e}")),
                }
            }
        }
    }
    Ok(report)
}

/// List projects (memory directories under the orca vault root).
#[orca_tool(domain = "namespace.project", verb = "list")]
async fn projects_list(
    _args: EmptyArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<ProjectsListReport> {
    let mut out = Vec::new();
    if ctx.config.memory_root.exists() {
        for entry in std::fs::read_dir(&ctx.config.memory_root)?.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                out.push(name.to_string());
            }
        }
        out.sort();
    }
    Ok(ProjectsListReport { projects: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::ToolCtx;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx_with_memory(root: PathBuf) -> ToolCtx {
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: root,
            db_path: PathBuf::from("/tmp/orca-tools-commands-test.db"),
            ports: Default::default(),
        }))
    }

    #[tokio::test]
    async fn projects_list_reads_memory_root_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        let ctx = ctx_with_memory(dir.path().to_path_buf());
        let r = projects_list(EmptyArgs {}, &ctx).await.unwrap();
        assert_eq!(r.projects, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[tokio::test]
    async fn projects_list_empty_when_root_missing() {
        let ctx = ctx_with_memory(PathBuf::from("/tmp/orca-nonexistent-memory-root"));
        let r = projects_list(EmptyArgs {}, &ctx).await.unwrap();
        assert!(r.projects.is_empty());
    }
}
