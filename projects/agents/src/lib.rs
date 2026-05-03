//! Agent prompt registry for the orca binary.
//!
//! Agent definitions are `.md` files with YAML frontmatter. They live in
//! `projects/agents/src/agents/` and are embedded at compile time by `build.rs`.
//! At runtime, `load_agent_prompt` tries the filesystem first so changes take
//! effect without rebuilding (set `BRAIN_AGENTS_DIR` to override the lookup path).

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
