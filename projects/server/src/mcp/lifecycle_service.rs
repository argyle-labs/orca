//! Server-side `LifecycleService` impl — install/uninstall/doctor/update/
//! projects-list/spec-dump.

use anyhow::Result;
use async_trait::async_trait;
use orca_tools_def::orca_lifecycle::{
    DoctorEntry, DoctorReport, LifecycleReport, ProjectsListReport, RuntimeSpecReport,
    SpecDumpReport, UpdateCheckReport,
};
use orca_tools_def::services::lifecycle::LifecycleService;
use orca_utils::config::Config;
use std::sync::Arc;

use crate::commands::install::{InstallReport, cmd_install_report, cmd_uninstall_report};
use crate::commands::update::{Channel, apply_update, check_for_update};

fn convert_install(rep: InstallReport) -> LifecycleReport {
    LifecycleReport {
        done: rep.done,
        skipped: rep.skipped,
        errors: rep.errors,
    }
}

pub struct ServerLifecycle {
    pub config: Arc<Config>,
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

        // Agents dir
        let agents_dir = cfg.agents_dir();
        let agent_files: Vec<_> = if agents_dir.exists() {
            std::fs::read_dir(&agents_dir)?
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                .collect()
        } else {
            push(
                &mut entries,
                "agents",
                "error",
                format!("agents dir missing: {}", agents_dir.display()),
            );
            vec![]
        };
        for entry in &agent_files {
            let path = entry.path();
            let stem = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let missing: Vec<&str> = [
                (!content.contains("name:")).then_some("name"),
                (!content.contains("description:")).then_some("description"),
                (!content.contains("tools:")).then_some("tools"),
            ]
            .into_iter()
            .flatten()
            .collect();
            if missing.is_empty() {
                push(
                    &mut entries,
                    "agent",
                    "ok",
                    format!("{stem}.md frontmatter present"),
                );
            } else {
                push(
                    &mut entries,
                    "agent",
                    "error",
                    format!("{stem}.md missing: {}", missing.join(", ")),
                );
            }
        }

        // Logs dir
        let logs_dir = cfg.logs_dir();
        if logs_dir.exists() {
            let test = logs_dir.join(".doctor_test");
            match std::fs::write(&test, "test") {
                Ok(_) => {
                    let _ = std::fs::remove_file(&test);
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

    async fn update_check(&self, channel: &str) -> Result<UpdateCheckReport> {
        let ch = Channel::parse(channel);
        let info = check_for_update(&ch).await?;
        Ok(UpdateCheckReport {
            channel: channel.into(),
            up_to_date: info.is_none(),
            latest: info.as_ref().map(|i| i.version.clone()),
            asset_url: info.as_ref().map(|i| i.asset_url.clone()),
        })
    }

    async fn update_apply(&self, channel: &str) -> Result<LifecycleReport> {
        let ch = Channel::parse(channel);
        let mut report = LifecycleReport {
            done: vec![],
            skipped: vec![],
            errors: vec![],
        };
        match check_for_update(&ch).await? {
            None => report
                .skipped
                .push(format!("already up to date on '{channel}'")),
            Some(info) => match apply_update(&info).await {
                Ok(()) => report.done.push(format!("updated to {}", info.version)),
                Err(e) => report.errors.push(format!("apply failed: {e}")),
            },
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
        Ok(RuntimeSpecReport {
            version: env!("CARGO_PKG_VERSION").into(),
            frontend: frontend.into(),
            target: env!("ORCA_BUILD_TARGET").into(),
        })
    }
}
