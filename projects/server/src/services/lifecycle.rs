//! Server-side `LifecycleService` impl — install/uninstall/doctor/update/
//! projects-list/spec-dump.

use anyhow::Result;
use async_trait::async_trait;
use auth::secrets::SecretsService;
use fleet::lifecycle::LifecycleService;
use fleet::lifecycle::{
    DoctorEntry, DoctorReport, LifecycleReport, ProjectsListReport, RuntimeSpecReport,
    SpecDumpReport,
};
use orca_utils::config::Config;
use std::sync::Arc;

use crate::commands::install::{InstallReport, cmd_install_report, cmd_uninstall_report};
use crate::commands::update::{
    apply_update, check_for_update, clear_version_pin, cmd_dev_enable, cmd_update_pin,
    resolve_channel, resolve_pin_veto, write_channel_marker,
};

/// Resolve the GitHub bearer token: prefer the `github_token` secret managed
/// by `SecretsService`; fall back to `GITHUB_TOKEN` env var for bootstrap.
async fn resolve_github_token(secrets: &Arc<dyn SecretsService>) -> Option<String> {
    if let Ok((_backend, v)) = secrets.get("github_token").await
        && !v.is_empty()
    {
        return Some(v);
    }
    std::env::var("GITHUB_TOKEN").ok().filter(|v| !v.is_empty())
}

fn convert_install(rep: InstallReport) -> LifecycleReport {
    LifecycleReport {
        done: rep.done,
        skipped: rep.skipped,
        errors: rep.errors,
    }
}

pub struct ServerLifecycle {
    pub config: Arc<Config>,
    pub secrets: Arc<dyn SecretsService>,
}

#[async_trait]
impl LifecycleService for ServerLifecycle {
    async fn install(&self) -> Result<LifecycleReport> {
        Ok(convert_install(cmd_install_report()))
    }

    async fn uninstall(&self) -> Result<LifecycleReport> {
        Ok(convert_install(cmd_uninstall_report()))
    }

    async fn doctor(&self) -> Result<DoctorReport> {
        let cfg = &self.config;
        let mut entries: Vec<DoctorEntry> = Vec::new();
        let push = |entries: &mut Vec<DoctorEntry>, cat: &str, status: &str, msg: String| {
            entries.push(DoctorEntry {
                category: cat.into(),
                status: status.into(),
                message: msg,
            });
        };

        // Vault
        if cfg.app_dir.exists() {
            push(
                &mut entries,
                "vault",
                "ok",
                format!("vault at {}", cfg.app_dir.display()),
            );
        } else {
            push(
                &mut entries,
                "vault",
                "error",
                format!("vault not found at {}", cfg.app_dir.display()),
            );
        }

        // Agents — served via the orca-local MCP. Embedded baseline is compiled
        // in at build time; frontmatter is enforced by build.rs, so a runtime
        // check is just a count + sanity ping. Any profile-level overrides are
        // listed too.
        let embedded = crate::agents::list_embedded_agents();
        push(
            &mut entries,
            "agents",
            "ok",
            format!("{} embedded agents available via MCP", embedded.len()),
        );
        for profile_dir in crate::services::agent_resolve::agent_search_dirs(cfg) {
            if !profile_dir.exists() {
                continue;
            }
            let count = std::fs::read_dir(&profile_dir)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                        .count()
                })
                .unwrap_or(0);
            push(
                &mut entries,
                "agents",
                "ok",
                format!("{count} profile overrides at {}", profile_dir.display()),
            );
        }

        // Logs dir
        let logs_dir = cfg.logs_dir();
        if logs_dir.exists() {
            let test = logs_dir.join(".doctor_test");
            match std::fs::write(&test, "test") {
                Ok(_) => {
                    _ = std::fs::remove_file(&test);
                    push(&mut entries, "logs", "ok", "logs dir writable".into());
                }
                Err(e) => push(
                    &mut entries,
                    "logs",
                    "error",
                    format!("logs dir not writable: {e}"),
                ),
            }
        } else {
            push(
                &mut entries,
                "logs",
                "error",
                format!("logs dir missing: {}", logs_dir.display()),
            );
        }

        // Memory root
        if cfg.memory_root.exists() {
            let n = std::fs::read_dir(&cfg.memory_root)?
                .flatten()
                .filter(|e| e.path().is_dir())
                .count();
            push(
                &mut entries,
                "memory",
                "ok",
                format!("memory root: {n} projects"),
            );
        } else {
            push(
                &mut entries,
                "memory",
                "error",
                format!("memory root missing: {}", cfg.memory_root.display()),
            );
        }

        // Anthropic API key
        if cfg.anthropic_api_key.is_some() {
            push(
                &mut entries,
                "auth",
                "ok",
                "anthropic key configured".into(),
            );
        } else {
            push(
                &mut entries,
                "auth",
                "warn",
                "anthropic key not set (escalation unavailable)".into(),
            );
        }

        Ok(DoctorReport { entries })
    }

    async fn set_version(&self, version: &str) -> Result<()> {
        match version {
            "dev" => {
                // Switch to dev channel: park the daemon and start cargo-watch.
                tokio::task::spawn_blocking(cmd_dev_enable).await??;
            }
            "stable" | "rc" => {
                let ch = resolve_channel(version);
                write_channel_marker(&ch)?;
                // Clear any semver pin so the channel takes effect.
                _ = clear_version_pin();
            }
            v => {
                // Treat as a semver pin.
                cmd_update_pin(v)?;
            }
        }
        Ok(())
    }

    async fn update_apply_current(&self) -> Result<LifecycleReport> {
        let ch = crate::commands::update::read_channel_marker()
            .unwrap_or_else(|| resolve_channel("stable"));
        let resolved = ch.as_marker();
        let mut report = LifecycleReport {
            done: vec![],
            skipped: vec![],
            errors: vec![],
        };
        let token = match resolve_github_token(&self.secrets).await {
            Some(t) => t,
            None => {
                report.errors.push(
                    "no github token — set secret 'github_token' or export GITHUB_TOKEN".into(),
                );
                return Ok(report);
            }
        };
        match check_for_update(&ch, &token).await? {
            None => report
                .skipped
                .push(format!("already up to date on '{resolved}'")),
            Some(info) => {
                if let Some(pin) = resolve_pin_veto(&info) {
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

    async fn projects_list(&self) -> Result<ProjectsListReport> {
        let mut out = Vec::new();
        if self.config.memory_root.exists() {
            for entry in std::fs::read_dir(&self.config.memory_root)?.flatten() {
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

    async fn spec_dump(&self) -> Result<SpecDumpReport> {
        let spec = crate::serve::openapi_spec_json();
        Ok(SpecDumpReport {
            spec: serde_json::to_string_pretty(&spec)?,
        })
    }

    async fn runtime_spec(&self) -> Result<RuntimeSpecReport> {
        let frontend = if cfg!(feature = "ui") {
            "embedded"
        } else {
            "disabled"
        };
        // A version string containing "-dev+" means this binary was built from
        // a git checkout past a release tag — always dev mode regardless of the
        // state file (which tracks the daemon supervisor's mode, not the binary).
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
        let channel =
            crate::commands::update::read_channel_marker().map(|c| c.as_marker().to_string());
        let pinned_to = crate::commands::update::read_version_pin();
        let system = Some((*crate::system_info::current_or_collect()).clone());
        Ok(RuntimeSpecReport {
            version: env!("ORCA_VERSION").into(),
            frontend: frontend.into(),
            target: env!("ORCA_BUILD_TARGET").into(),
            mode,
            channel,
            pinned_to,
            system,
        })
    }
}
