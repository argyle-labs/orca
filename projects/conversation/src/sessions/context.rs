use anyhow::Result;
use contract::config::Config;

/// Resolved project context: system prompt + memory content.
#[derive(Debug, Default)]
pub struct ProjectContext {
    pub project: Option<String>,
    pub memory_content: Option<String>,
}

impl ProjectContext {
    /// Try to resolve a project name to its memory dir.
    /// Matches: `<name>` → `~/.orca/memory/<name>/MEMORY.md`
    pub fn resolve(name: &str, config: &Config) -> Result<Self> {
        let memory_root = &config.memory_root;

        // Exact match first
        let exact = memory_root.join(name).join("MEMORY.md");
        if exact.exists() {
            let content = std::fs::read_to_string(&exact)?;
            return Ok(ProjectContext {
                project: Some(name.to_string()),
                memory_content: Some(content),
            });
        }

        // Fuzzy: find any dir that contains the name as a substring
        if let Ok(entries) = std::fs::read_dir(memory_root) {
            for entry in entries.flatten() {
                let dir_name = entry.file_name();
                let dir_name = dir_name.to_string_lossy();
                if dir_name.contains(name) {
                    let memory_file = entry.path().join("MEMORY.md");
                    if memory_file.exists() {
                        let content = std::fs::read_to_string(&memory_file)?;
                        return Ok(ProjectContext {
                            project: Some(dir_name.to_string()),
                            memory_content: Some(content),
                        });
                    }
                }
            }
        }

        // No match — return empty context (general session)
        Ok(ProjectContext {
            project: Some(name.to_string()),
            ..Default::default()
        })
    }

    /// Build the system prompt for this context.
    /// Loads Wolf's agent definition (filesystem first, embedded fallback) + injects memory.
    pub fn build_system_prompt(&self, config: &Config) -> String {
        self.build_system_prompt_for_backend(config, true)
    }

    /// Build a system prompt appropriate for the backend capability level.
    /// `full_persona` = true: full Wolf prompt with agent routing (Claude, capable local models).
    /// `full_persona` = false: stripped-down prompt for local models that don't handle complex personas.
    pub fn build_system_prompt_for_backend(&self, config: &Config, full_persona: bool) -> String {
        let base = if full_persona {
            agents::resolve::load_agent_prompt("wolf", config).unwrap_or_else(|| {
                eprintln!("warning: wolf.md not found — using minimal fallback prompt");
                "You are an AI assistant. Be precise, efficient, and honest.".to_string()
            })
        } else {
            // Local models (LMStudio, Ollama) get a clean minimal prompt — not a stripped Wolf.
            // The Wolf persona (Otter narration, agent routing) causes them to loop and narrate.
            local_model_prompt()
        };

        if let Some(memory) = &self.memory_content {
            format!(
                "{}\n\n---\n\n## Project Context\n\nProject: {}\n\n{memory}",
                base,
                self.project.as_deref().unwrap_or("unknown"),
            )
        } else {
            base
        }
    }
}

/// Minimal system prompt for local models (LM Studio, Ollama).
/// The full Wolf persona (Otter narration, agent routing) confuses local models — they narrate
/// to themselves, loop, and repeat. This gives them a clean, direct instruction set instead.
fn local_model_prompt() -> String {
    "You are a helpful AI assistant. Be concise, accurate, and direct.\n\
     \n\
     Rules:\n\
     - Answer the question. Do not explain what you are about to do — just do it.\n\
     - No preamble. No narration. No reasoning out loud. State your answer directly.\n\
     - Do not repeat yourself.\n\
     - If you do not know something, say so in one sentence.\n\
     - Use tools when they would help answer the question more accurately."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::Model;
    use std::path::PathBuf;

    fn test_config(memory_root: PathBuf) -> Config {
        Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root,
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        }
    }

    // ── ProjectContext::resolve ───────────────────────────────────────────────

    #[test]
    fn resolve_exact_match_loads_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_root = tmp.path().to_path_buf();
        let project_dir = memory_root.join("myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("MEMORY.md"), "# My Project Memory").unwrap();

        let config = test_config(memory_root);
        let ctx = ProjectContext::resolve("myproject", &config).unwrap();

        assert_eq!(ctx.project.as_deref(), Some("myproject"));
        assert_eq!(ctx.memory_content.as_deref(), Some("# My Project Memory"));
    }

    #[test]
    fn resolve_fuzzy_match_finds_partial_name() {
        let tmp = tempfile::tempdir().unwrap();
        let memory_root = tmp.path().to_path_buf();
        let project_dir = memory_root.join("myproject-backend");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("MEMORY.md"), "backend memory").unwrap();

        let config = test_config(memory_root);
        let ctx = ProjectContext::resolve("backend", &config).unwrap();

        assert!(
            ctx.memory_content.is_some(),
            "fuzzy match should load memory"
        );
    }

    #[test]
    fn resolve_no_match_returns_empty_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path().to_path_buf());

        let ctx = ProjectContext::resolve("nonexistent-project-xyz", &config).unwrap();

        assert_eq!(ctx.project.as_deref(), Some("nonexistent-project-xyz"));
        assert!(
            ctx.memory_content.is_none(),
            "no match should have no memory"
        );
    }

    #[test]
    fn resolve_empty_memory_root_returns_gracefully() {
        let config = test_config(PathBuf::from("/tmp/__no_such_memory_root__"));
        let ctx = ProjectContext::resolve("anything", &config).unwrap();
        assert!(ctx.memory_content.is_none());
    }

    // ── build_system_prompt ───────────────────────────────────────────────────

    #[test]
    fn build_system_prompt_without_memory_returns_wolf_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        let ctx = ProjectContext {
            project: None,
            memory_content: None,
        };

        let prompt = ctx.build_system_prompt(&config);
        // No memory — just the wolf prompt (or fallback)
        assert!(!prompt.is_empty());
        assert!(
            !prompt.contains("Project Context"),
            "no memory means no project section"
        );
    }

    #[test]
    fn build_system_prompt_with_memory_includes_context_section() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        let ctx = ProjectContext {
            project: Some("myproject".into()),
            memory_content: Some("Key facts here.".into()),
        };

        let prompt = ctx.build_system_prompt(&config);
        assert!(
            prompt.contains("Project Context"),
            "memory should add project context section"
        );
        assert!(prompt.contains("myproject"), "project name should appear");
        assert!(
            prompt.contains("Key facts here."),
            "memory content should be included"
        );
    }

    #[test]
    fn build_system_prompt_memory_appended_after_wolf() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        let ctx = ProjectContext {
            project: Some("proj".into()),
            memory_content: Some("mem content".into()),
        };

        let prompt = ctx.build_system_prompt(&config);
        let wolf_end = prompt.find("---").unwrap_or(0);
        let mem_start = prompt.find("mem content").unwrap_or(usize::MAX);
        assert!(
            wolf_end < mem_start,
            "wolf prompt should come before memory injection"
        );
    }
}
