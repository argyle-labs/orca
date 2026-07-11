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

    // Core embeds no base roster: the roster is supplied by the external
    // `argyle-labs/agents` plugin via the registration seam. Report the composed
    // count so the diagnostic reflects what is actually available at runtime.
    let composed = agents::compose_agents().len();
    push(
        &mut entries,
        "agents",
        "ok",
        format!("{composed} agents available via MCP (roster supplied by plugin)"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{Model, Ports};
    use std::path::PathBuf;

    fn find<'a>(entries: &'a [DoctorEntry], category: &str) -> Vec<&'a DoctorEntry> {
        entries.iter().filter(|e| e.category == category).collect()
    }

    /// Build a Config rooted at `app_dir` / `memory_root`, with no API key.
    fn cfg(app_dir: PathBuf, memory_root: PathBuf, api_key: Option<String>) -> Config {
        Config {
            anthropic_api_key: api_key,
            lmstudio_url: String::new(),
            ollama_url: String::new(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            db_path: app_dir.join("orca.db"),
            app_dir,
            memory_root,
            ports: Ports::default(),
        }
    }

    #[test]
    fn doctor_entry_serde_roundtrips() {
        let e = DoctorEntry {
            category: "vault".into(),
            status: "ok".into(),
            message: "hi".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: DoctorEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.category, "vault");
        assert_eq!(back.status, "ok");
        assert_eq!(back.message, "hi");
    }

    #[test]
    fn missing_vault_and_memory_and_logs_are_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // app_dir does NOT exist (subdir under tmp that we never create), and
        // memory_root missing too.
        let app = tmp.path().join("no-vault");
        let mem = tmp.path().join("no-memory");
        let entries = collect(&cfg(app, mem, None)).unwrap();

        let vault = find(&entries, "vault");
        assert_eq!(vault.len(), 1);
        assert_eq!(vault[0].status, "error");
        assert!(vault[0].message.contains("vault not found"));

        // logs_dir is app_dir/logs/sessions — absent because app_dir is absent.
        let logs = find(&entries, "logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, "error");
        assert!(logs[0].message.contains("logs dir missing"));

        let memory = find(&entries, "memory");
        assert_eq!(memory.len(), 1);
        assert_eq!(memory[0].status, "error");
        assert!(memory[0].message.contains("memory root missing"));
    }

    #[test]
    fn present_vault_logs_memory_are_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vault");
        std::fs::create_dir_all(app.join("logs/sessions")).unwrap();
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(mem.join("proj-a")).unwrap();
        std::fs::create_dir_all(mem.join("proj-b")).unwrap();

        let entries = collect(&cfg(app, mem, None)).unwrap();

        let vault = find(&entries, "vault");
        assert_eq!(vault[0].status, "ok");
        assert!(vault[0].message.contains("vault at"));

        let logs = find(&entries, "logs");
        assert_eq!(logs[0].status, "ok");
        assert_eq!(logs[0].message, "logs dir writable");

        let memory = find(&entries, "memory");
        assert_eq!(memory[0].status, "ok");
        assert!(memory[0].message.contains("2 projects"));
    }

    #[test]
    fn agents_entry_present_and_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vault");
        std::fs::create_dir_all(&app).unwrap();
        let entries = collect(&cfg(app, tmp.path().join("m"), None)).unwrap();

        // Core embeds no roster; the entry reports the composed count and notes
        // the roster is supplied by the external plugin.
        let agents_entry = find(&entries, "agents")
            .into_iter()
            .find(|e| e.message.contains("agents available via MCP"));
        assert!(agents_entry.is_some());
        assert_eq!(agents_entry.unwrap().status, "ok");
    }

    #[test]
    fn auth_warn_when_key_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vault");
        std::fs::create_dir_all(&app).unwrap();
        let entries = collect(&cfg(app, tmp.path().join("m"), None)).unwrap();
        let auth = find(&entries, "auth");
        assert_eq!(auth.len(), 1);
        assert_eq!(auth[0].status, "warn");
        assert!(auth[0].message.contains("not set"));
    }

    #[test]
    fn auth_ok_when_key_present() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vault");
        std::fs::create_dir_all(&app).unwrap();
        let entries = collect(&cfg(app, tmp.path().join("m"), Some("sk-x".into()))).unwrap();
        let auth = find(&entries, "auth");
        assert_eq!(auth[0].status, "ok");
        assert_eq!(auth[0].message, "anthropic key configured");
    }

    #[test]
    fn memory_root_counts_only_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("vault");
        std::fs::create_dir_all(&app).unwrap();
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(mem.join("only-dir")).unwrap();
        std::fs::write(mem.join("loose-file.md"), "x").unwrap();

        let entries = collect(&cfg(app, mem, None)).unwrap();
        let memory = find(&entries, "memory");
        assert!(memory[0].message.contains("1 projects"));
    }
}
