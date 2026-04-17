use crate::config::Config;
use anyhow::Result;

/// Resolved project context: system prompt + memory content.
#[derive(Debug, Default)]
pub struct ProjectContext {
    pub project: Option<String>,
    pub memory_content: Option<String>,
}

impl ProjectContext {
    /// Try to resolve a project name to its memory dir.
    /// Matches: "halvor" → ~/brain/ai/claude/memory/halvor/MEMORY.md
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
    /// Loads Wolf's agent definition + injects memory if present.
    pub fn build_system_prompt(&self, config: &Config) -> String {
        let wolf_path = config.agents_dir().join("wolf.md");

        let wolf_prompt = if wolf_path.exists() {
            // Strip YAML frontmatter (everything between --- lines)
            match std::fs::read_to_string(&wolf_path) {
                Ok(content) => strip_frontmatter(&content),
                Err(_) => default_wolf_prompt(),
            }
        } else {
            default_wolf_prompt()
        };

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

fn strip_frontmatter(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) == Some("---") {
        // Find closing ---
        if let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
            return lines[end + 2..].join("\n").trim().to_string();
        }
    }
    content.trim().to_string()
}

fn default_wolf_prompt() -> String {
    // wolf.md not found in the brain vault — minimal fallback
    // Real prompt lives at: ~/brain/ai/claude/agents/wolf.md
    eprintln!("warning: wolf.md not found in agents dir — using minimal fallback prompt");
    "You are an AI assistant. Be precise, efficient, and honest.".to_string()
}
