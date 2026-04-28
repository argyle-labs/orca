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

/// Install all embedded agents into `target_dir`, removing orphans from previous installs.
///
/// A manifest file (`.brain-agents`) tracks which names were installed by brain so we never
/// touch user-created agent files.
pub fn install_agents(target_dir: &Path) -> anyhow::Result<InstallReport> {
    std::fs::create_dir_all(target_dir)?;

    let manifest_path = target_dir.join(".brain-agents");
    let old_names = read_manifest(&manifest_path);
    let new_names: Vec<&str> = embedded_agent_names().to_vec();

    // Remove agents that were installed by a previous version but are gone now.
    let mut removed = vec![];
    for old in &old_names {
        if !new_names.contains(&old.as_str()) {
            let stale = target_dir.join(format!("{old}.md"));
            if stale.exists() {
                std::fs::remove_file(&stale)?;
                removed.push(old.clone());
            }
        }
    }

    // Write all current embedded agents.
    let mut written = vec![];
    let mut unchanged = 0usize;
    for name in &new_names {
        let raw = match embedded_agent(name) {
            Some(r) => r,
            None => continue,
        };
        let dest = target_dir.join(format!("{name}.md"));
        let existing = std::fs::read_to_string(&dest).unwrap_or_default();
        if existing == raw {
            unchanged += 1;
        } else {
            std::fs::write(&dest, raw)?;
            written.push(name.to_string());
        }
    }

    // Write new manifest.
    let manifest_content = new_names.join("\n") + "\n";
    std::fs::write(&manifest_path, manifest_content)?;

    Ok(InstallReport {
        written,
        removed,
        unchanged,
    })
}

pub struct InstallReport {
    pub written: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

fn read_manifest(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
