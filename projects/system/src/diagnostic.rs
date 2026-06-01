//! Host diagnostic entries, surfaced as the `diagnostic` field of
//! `system.detail`. There is no standalone `system.diagnostic` orca_tool
//! — diagnostics are a detail of the system, not a separate resource.

use anyhow::Result;
use contract::config::Config;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct DoctorEntry {
    pub category: String,
    pub status: String, // "ok" | "warn" | "error"
    pub message: String,
}

/// Validate agent files, symlinks, config, tool availability. Returns the
/// list of ok/warn/error entries that `system.detail` exposes.
pub(crate) fn collect(cfg: &Config) -> Result<Vec<DoctorEntry>> {
    let mut entries: Vec<DoctorEntry> = Vec::new();
    let push = |entries: &mut Vec<DoctorEntry>, cat: &str, status: &str, msg: String| {
        entries.push(DoctorEntry {
            category: cat.into(),
            status: status.into(),
            message: msg,
        });
    };

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

    let embedded = agents::embedded::list_embedded_agents();
    push(
        &mut entries,
        "agents",
        "ok",
        format!("{} embedded agents available via MCP", embedded.len()),
    );
    for profile_dir in agents::resolve::agent_search_dirs(cfg) {
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

    Ok(entries)
}
