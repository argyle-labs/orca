//! Agent prompt registry for the orca binary.
//!
//! Agent definitions are `.md` files with YAML frontmatter. Core embeds no base
//! roster of its own — the full roster (wolf/otter/…) lives in the external
//! `argyle-labs/agents` plugin and is registered at runtime through the
//! `plugin_toolkit::agents` seam. At runtime, `load_agent_prompt` resolves
//! prompts from the filesystem (set `ORCA_AGENTS_DIR` to override the lookup
//! path); the embedded lookups below always resolve to nothing.

// Generated at build time by build.rs — an empty embedded lookup table. Core
// carries no embedded agent fallback; the external plugin supplies the roster.
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

use crate::registry::AgentDef;
use std::path::{Path, PathBuf};

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
// The composition registry (see `registry.rs`) is the abstraction; the provider
// below is a concrete source that feeds it. Core no longer bridges a compiled-in
// base roster — the roster is supplied by the external `argyle-labs/agents`
// plugin, which registers itself against this same registry. `compose_agents()`
// remains the single source of truth for `orca install` and the internal chat
// roster alike.

use crate::registry::AgentProvider;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── Embedded agents ───────────────────────────────────────────────────────

    #[test]
    fn core_embeds_no_base_roster() {
        // Core carries no embedded agent fallback: the roster is supplied by the
        // external `argyle-labs/agents` plugin via the registration seam.
        assert!(
            list_embedded_agents().is_empty(),
            "core must not embed any base agents"
        );
        assert!(embedded_agent_names().is_empty());
        assert!(embedded_agent("wolf").is_none());
        assert!(embedded_agent_raw("wolf").is_none());
    }

    #[test]
    fn load_agent_prompt_reads_from_filesystem() {
        let dir = std::env::temp_dir().join(format!("orca_agent_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let content = "---\ndescription: override\n---\nPrompt from filesystem!";
        fs::write(dir.join("orca.md"), content).unwrap();

        let prompt = load_agent_prompt("orca", &dir).unwrap();
        assert_eq!(prompt, "Prompt from filesystem!");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_agent_prompt_unknown_agent_returns_none() {
        // With no embedded fallback, a missing filesystem file yields None.
        let nonexistent = PathBuf::from("/tmp/__orca_no_such_dir__");
        assert!(load_agent_prompt("zzz_nonexistent_agent_xyz", &nonexistent).is_none());
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
