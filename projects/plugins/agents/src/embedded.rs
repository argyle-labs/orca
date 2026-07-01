//! Agent prompt registry for the orca binary.
//!
//! Agent definitions are `.md` files with YAML frontmatter. They live in
//! `projects/agents/src/agents/` and are embedded at compile time by `build.rs`.
//! At runtime, `load_agent_prompt` tries the filesystem first so changes take
//! effect without rebuilding (set `ORCA_AGENTS_DIR` to override the lookup path).

// Generated at build time by build.rs — embeds agent .md files into the binary.
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

use crate::registry::{AgentDef, AgentProvider, register_provider};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Load an agent prompt: try filesystem first (hot-reload during dev), fall back to embedded.
pub fn load_agent_prompt(name: &str, agents_dir: &Path) -> Option<String> {
    load_agent_prompt_from_dirs(name, &[agents_dir])
}

/// Load an agent prompt searching multiple filesystem directories in priority
/// order, falling back to the embedded agent. The first directory that
/// contains a readable `<name>.md` wins.
///
/// Intended caller order: profile agents dir first (highest priority for
/// personal/shared agents), then dev-override dir (e.g. `~/.claude/agents`),
/// then embedded baseline.
pub fn load_agent_prompt_from_dirs(name: &str, dirs: &[&Path]) -> Option<String> {
    for dir in dirs {
        let path = dir.join(format!("{name}.md"));
        if path.exists()
            && let Ok(raw) = std::fs::read_to_string(&path)
        {
            return Some(strip_frontmatter(&raw));
        }
    }
    embedded_agent(name).map(strip_frontmatter)
}

/// Read raw embedded agent content (frontmatter intact) for a known agent name.
/// Used by migrators that need to seed a profile from the embedded baseline.
pub fn embedded_agent_raw(name: &str) -> Option<&'static str> {
    embedded_agent(name)
}

/// All embedded agents with their name and description (parsed from frontmatter).
pub fn list_embedded_agents() -> Vec<(String, String)> {
    embedded_agent_names()
        .iter()
        .filter_map(|name| {
            let raw = embedded_agent(name)?;
            let desc = frontmatter_field_from_str(raw, "description").unwrap_or_default();
            Some((name.to_string(), desc))
        })
        .collect()
}

fn frontmatter_field_from_str(content: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}:");
    content
        .lines()
        .find_map(|l| l.strip_prefix(&prefix).map(|v| v.trim().to_string()))
}

fn strip_frontmatter(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) == Some("---")
        && let Some(end) = lines[1..].iter().position(|l| l.trim() == "---")
    {
        return lines[end + 2..].join("\n").trim().to_string();
    }
    content.trim().to_string()
}

// ── Provider bridges ──────────────────────────────────────────────────────────
//
// The composition registry (see `registry.rs`) is the abstraction; the sources
// below are two concrete providers that feed it. Until the base roster moves to
// an external `argyle-labs/agents` plugin, these bridge orca's compiled-in and
// filesystem rosters into the same registry every plugin registers against, so
// `compose_agents()` is the single source of truth for `orca install` and the
// internal chat roster alike.

/// The compiled-in base roster (wolf/otter/…), exposed as an [`AgentProvider`]
/// so core composition treats it exactly like a loaded plugin. When the roster
/// is extracted to `argyle-labs/agents`, delete this and let the plugin
/// register itself — nothing else changes.
pub struct BaseRosterProvider;

impl AgentProvider for BaseRosterProvider {
    fn name(&self) -> &str {
        "orca-embedded-roster"
    }

    fn agents(&self) -> Vec<AgentDef> {
        embedded_agent_names()
            .iter()
            .filter_map(|name| {
                let raw = embedded_agent(name)?;
                Some(AgentDef {
                    name: name.to_string(),
                    body: raw.to_string(),
                    origin: "embedded".to_string(),
                })
            })
            .collect()
    }
}

/// A directory of `<name>.md` agent files surfaced as an [`AgentProvider`].
/// Used to bridge external source repos that own their own rosters into the
/// registry without special-casing them in the composer.
pub struct FsRosterProvider {
    origin: String,
    dir: PathBuf,
}

impl FsRosterProvider {
    pub fn new(origin: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        FsRosterProvider {
            origin: origin.into(),
            dir: dir.into(),
        }
    }
}

impl AgentProvider for FsRosterProvider {
    fn name(&self) -> &str {
        &self.origin
    }

    fn agents(&self) -> Vec<AgentDef> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_suffix(".md"))?;
                let body = std::fs::read_to_string(&path).ok()?;
                Some(AgentDef {
                    name: name.to_string(),
                    body,
                    origin: self.origin.clone(),
                })
            })
            .collect()
    }
}

/// Register the compiled-in base roster into the process-global provider
/// registry. Idempotent — [`register_provider`] replaces by provider name.
pub fn register_base_roster() {
    register_provider(Arc::new(BaseRosterProvider));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── Embedded agents ───────────────────────────────────────────────────────

    #[test]
    fn list_embedded_agents_is_non_empty() {
        let agents = list_embedded_agents();
        assert!(
            !agents.is_empty(),
            "at least one agent must be embedded at build time"
        );
    }

    #[test]
    fn list_embedded_agents_all_have_non_empty_name_and_description() {
        for (name, desc) in list_embedded_agents() {
            assert!(!name.is_empty(), "agent name must not be empty");
            assert!(!desc.is_empty(), "agent '{name}' has empty description");
        }
    }

    #[test]
    fn load_agent_prompt_known_embedded_agent() {
        // Use the first embedded agent name — guaranteed to exist at build time.
        let first_name = list_embedded_agents()
            .into_iter()
            .next()
            .expect("at least one embedded agent")
            .0;
        let nonexistent = PathBuf::from("/tmp/__orca_no_such_dir__");
        let prompt = load_agent_prompt(&first_name, &nonexistent);
        assert!(
            prompt.is_some(),
            "embedded agent '{first_name}' should always load"
        );
        let text = prompt.unwrap();
        assert!(
            !text.is_empty(),
            "prompt should not be empty after stripping frontmatter"
        );
        // Verify the opening frontmatter delimiter is gone (body may contain --- as markdown)
        assert!(
            !text.trim_start().starts_with("---"),
            "opening frontmatter delimiter should be stripped"
        );
    }

    #[test]
    fn load_agent_prompt_unknown_agent_returns_none() {
        let nonexistent = PathBuf::from("/tmp/__orca_no_such_dir__");
        assert!(load_agent_prompt("zzz_nonexistent_agent_xyz", &nonexistent).is_none());
    }

    #[test]
    fn load_agent_prompt_prefers_filesystem_over_embedded() {
        let dir = std::env::temp_dir().join(format!("orca_agent_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let content = "---\ndescription: override\n---\nOverride prompt from filesystem!";
        fs::write(dir.join("orca.md"), content).unwrap();

        let prompt = load_agent_prompt("orca", &dir).unwrap();
        assert_eq!(prompt, "Override prompt from filesystem!");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_agent_prompt_falls_back_to_embedded_when_file_missing() {
        let first_name = list_embedded_agents()
            .into_iter()
            .next()
            .expect("at least one embedded agent")
            .0;
        let dir = std::env::temp_dir().join(format!("orca_agent_fallback_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Dir exists but the agent .md file is not present — should fall back to embedded.
        let prompt = load_agent_prompt(&first_name, &dir);
        assert!(
            prompt.is_some(),
            "should fall back to embedded agent '{first_name}'"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ── strip_frontmatter ─────────────────────────────────────────────────────

    #[test]
    fn strip_frontmatter_removes_yaml_block() {
        let raw = "---\nname: test\ndescription: stuff\n---\nBody content here.";
        assert_eq!(strip_frontmatter(raw), "Body content here.");
    }

    #[test]
    fn strip_frontmatter_no_frontmatter_passthrough() {
        let raw = "Just a plain prompt with no frontmatter.";
        assert_eq!(strip_frontmatter(raw), raw);
    }

    #[test]
    fn strip_frontmatter_multiline_body() {
        let raw = "---\ndescription: foo\n---\nLine 1.\nLine 2.\nLine 3.";
        assert_eq!(strip_frontmatter(raw), "Line 1.\nLine 2.\nLine 3.");
    }

    #[test]
    fn strip_frontmatter_empty_body() {
        let raw = "---\ndescription: empty\n---\n";
        assert_eq!(strip_frontmatter(raw), "");
    }

    // ── frontmatter_field_from_str ────────────────────────────────────────────

    #[test]
    fn frontmatter_field_extracts_description() {
        let raw = "---\nname: orca\ndescription: The main agent\n---\nBody.";
        let desc = frontmatter_field_from_str(raw, "description");
        assert_eq!(desc.as_deref(), Some("The main agent"));
    }

    #[test]
    fn frontmatter_field_returns_none_for_missing_field() {
        let raw = "---\nname: orca\n---\nBody.";
        assert!(frontmatter_field_from_str(raw, "description").is_none());
    }

    // ── Provider bridges ──────────────────────────────────────────────────────

    #[test]
    fn base_roster_provider_exposes_embedded_agents() {
        let provider = BaseRosterProvider;
        let agents = provider.agents();
        assert_eq!(
            agents.len(),
            embedded_agent_names().len(),
            "base roster must surface every embedded agent"
        );
        assert!(agents.iter().all(|a| a.origin == "embedded"));
        assert!(agents.iter().all(|a| !a.body.is_empty()));
    }

    #[test]
    fn fs_roster_provider_reads_md_files() {
        let dir = std::env::temp_dir().join(format!("orca_fs_roster_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("falcon.md"), "---\nname: falcon\n---\nBody.").unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let provider = FsRosterProvider::new("~/code/test-repo", &dir);
        let agents = provider.agents();
        assert_eq!(agents.len(), 1, "only .md files become agents");
        assert_eq!(agents[0].name, "falcon");
        assert_eq!(agents[0].origin, "~/code/test-repo");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fs_roster_provider_missing_dir_is_empty() {
        let provider = FsRosterProvider::new("~/code/nope", "/tmp/__orca_no_such_roster__");
        assert!(provider.agents().is_empty());
    }
}
