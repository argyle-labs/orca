//! Agent prompt registry for the orca binary.
//!
//! Agent definitions are `.md` files with YAML frontmatter. They live in
//! `projects/agents/src/agents/` and are embedded at compile time by `build.rs`.
//! At runtime, `load_agent_prompt` tries the filesystem first so changes take
//! effect without rebuilding (set `ORCA_AGENTS_DIR` to override the lookup path).

// Generated at build time by build.rs — embeds agent .md files into the binary.
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

use std::path::Path;

/// Load an agent prompt: try filesystem first (hot-reload during dev), fall back to embedded.
pub fn load_agent_prompt(name: &str, agents_dir: &Path) -> Option<String> {
    let path = agents_dir.join(format!("{name}.md"));
    if path.exists()
        && let Ok(raw) = std::fs::read_to_string(&path)
    {
        return Some(strip_frontmatter(&raw));
    }
    embedded_agent(name).map(strip_frontmatter)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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
}
