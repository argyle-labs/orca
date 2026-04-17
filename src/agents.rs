// Generated at build time by build.rs — embeds agent .md files into the binary.
include!(concat!(env!("OUT_DIR"), "/embedded_agents.rs"));

/// Load an agent prompt: try embedded first, fall back to filesystem.
pub fn load_agent_prompt(name: &str, agents_dir: &std::path::Path) -> Option<String> {
    // Filesystem takes priority (allows hot-reloading during dev)
    let path = agents_dir.join(format!("{name}.md"));
    if path.exists()
        && let Ok(raw) = std::fs::read_to_string(&path) {
        return Some(strip_frontmatter(&raw));
    }

    // Fall back to embedded (works without ~/brain at runtime)
    embedded_agent(name).map(strip_frontmatter)
}

fn strip_frontmatter(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.first().map(|l| l.trim()) == Some("---")
        && let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") {
        return lines[end + 2..].join("\n").trim().to_string();
    }
    content.trim().to_string()
}
