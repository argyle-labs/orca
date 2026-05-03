use brain_utils::config::Config;
use anyhow::Result;

/// Resolved project context: system prompt + memory content.
#[derive(Debug, Default)]
pub struct ProjectContext {
    pub project: Option<String>,
    pub memory_content: Option<String>,
}

impl ProjectContext {
    /// Try to resolve a project name to its memory dir.
    /// Matches: "halvor" → ~/orca/memory/halvor/MEMORY.md
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
                if dir_name.contains(name) && !dir_name.starts_with("rebuy") {
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
        let wolf_prompt = brain_agents::load_agent_prompt("wolf", &config.agents_dir())
            .unwrap_or_else(|| {
                eprintln!("warning: wolf.md not found — using minimal fallback prompt");
                "You are an AI assistant. Be precise, efficient, and honest.".to_string()
            });

        if let Some(memory) = &self.memory_content {
            format!(
                "{}\n\n---\n\n## Project Context\n\nProject: {}\n\n{memory}",
                wolf_prompt,
                self.project.as_deref().unwrap_or("unknown"),
            )
        } else {
            wolf_prompt
        }
    }
}
