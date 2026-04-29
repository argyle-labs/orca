use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, Clone, ToSchema)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<TreeNode>>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    File,
    Dir,
}

pub fn get_roots() -> HashMap<String, PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut roots = HashMap::new();
    roots.insert(
        "rebuy".to_string(),
        PathBuf::from(std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"))),
    );
    roots.insert("brain".to_string(), PathBuf::from(format!("{home}/brain")));
    roots.insert("claude".to_string(), PathBuf::from(format!("{home}/.claude")));
    roots.insert(
        "dotfiles".to_string(),
        PathBuf::from(
            std::env::var("DOTFILES_ROOT").unwrap_or_else(|_| format!("{home}/dotfiles")),
        ),
    );
    roots.insert(
        "teaching".to_string(),
        PathBuf::from(format!("{home}/brain/ai/claude/teaching")),
    );
    roots
}

pub fn get_ignored(root_name: &str) -> HashSet<String> {
    match root_name {
        "rebuy" => [
            "node_modules",
            ".git",
            ".next",
            "dist",
            "build",
            "vendor",
            "www",
            "docs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        "brain" => [
            ".git",
            "logs",
            "memory",
            "plugins",
            ".trash",
            "node_modules",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        "teaching" => [".git"].iter().map(|s| s.to_string()).collect(),
        _ => HashSet::new(),
    }
}

// Search intentionally includes memory/ (unlike the nav tree) so Claude can
// find relevant context across past decisions without exposing the raw tree.
pub fn get_search_ignored(root_name: &str) -> HashSet<String> {
    match root_name {
        "rebuy" => [
            "node_modules",
            ".git",
            ".next",
            "dist",
            "build",
            "vendor",
            "www",
            "docs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        "brain" => [".git", "logs", ".trash", "node_modules", "plugins"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        _ => HashSet::new(),
    }
}

fn extract_title(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    // Prefer frontmatter `name:` — authoritative for agent/command files
    if let Some(rest) = content.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        for line in rest[..end].lines() {
            if let Some(val) = line.strip_prefix("name:") {
                let name = val.trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
    }
    // Fall back to first H1 heading
    content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l[2..].trim().to_string())
}

fn is_dir(full: &Path) -> bool {
    if let Ok(meta) = full.metadata() {
        return meta.is_dir();
    }
    // Handle broken symlinks gracefully
    false
}

pub fn build_tree_raw(dir: &Path, root_dir: &Path, ignored: &HashSet<String>) -> Vec<TreeNode> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || ignored.contains(&name) {
            continue;
        }
        let full = entry.path();
        let rel = full
            .strip_prefix(root_dir)
            .unwrap_or(&full)
            .to_string_lossy()
            .to_string();

        if is_dir(&full) {
            let children = build_tree_raw(&full, root_dir, ignored);
            if !children.is_empty() {
                nodes.push(TreeNode {
                    name,
                    path: rel,
                    node_type: NodeType::Dir,
                    children: Some(children),
                });
            }
        } else {
            let ext = full.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "md" || ext == "mdx" {
                let stem = full
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&name)
                    .to_string();
                let title = extract_title(&full).unwrap_or(stem);
                nodes.push(TreeNode {
                    name: title,
                    path: rel,
                    node_type: NodeType::File,
                    children: None,
                });
            }
        }
    }
    nodes.sort_by(|a, b| match (&a.node_type, &b.node_type) {
        (NodeType::Dir, NodeType::File) => std::cmp::Ordering::Less,
        (NodeType::File, NodeType::Dir) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    nodes
}

fn count_files(nodes: &[TreeNode]) -> usize {
    nodes
        .iter()
        .map(|n| match &n.children {
            None => 1,
            Some(c) => count_files(c),
        })
        .sum()
}

fn find_single_file(nodes: &[TreeNode]) -> Option<TreeNode> {
    for node in nodes {
        if node.node_type == NodeType::File {
            return Some(node.clone());
        }
        if let Some(ref children) = node.children
            && let Some(f) = find_single_file(children)
        {
            return Some(f);
        }
    }
    None
}

fn compact_tree(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    let mut result = Vec::new();
    for node in nodes {
        if node.node_type == NodeType::File {
            result.push(node);
            continue;
        }
        let children = compact_tree(node.children.unwrap_or_default());
        if count_files(&children) == 1
            && let Some(f) = find_single_file(&children)
        {
            result.push(f);
            continue;
        }
        if children.len() == 1 && children[0].node_type == NodeType::Dir {
            let child = children.into_iter().next().unwrap();
            result.push(TreeNode {
                name: format!("{}/{}", node.name, child.name),
                ..child
            });
            continue;
        }
        result.push(TreeNode {
            children: Some(children),
            ..node
        });
    }
    result
}

pub fn get_root_tree(root_name: &str) -> Vec<TreeNode> {
    let roots = get_roots();
    let Some(root_dir) = roots.get(root_name) else {
        return vec![];
    };
    let ignored = get_ignored(root_name);
    compact_tree(build_tree_raw(root_dir, root_dir, &ignored))
}

pub fn collect_all_files(nodes: &[TreeNode]) -> Vec<TreeNode> {
    let mut files = Vec::new();
    for node in nodes {
        match node.node_type {
            NodeType::File => files.push(node.clone()),
            NodeType::Dir => files.extend(collect_all_files(
                node.children.as_deref().unwrap_or_default(),
            )),
        }
    }
    files
}

#[allow(dead_code)]
pub fn resolve_file(root_name: &str, doc_path: &str) -> Option<PathBuf> {
    let roots = get_roots();
    let root_dir = roots.get(root_name)?;
    for ext in &[".md", ".mdx", ""] {
        let full = root_dir.join(format!("{doc_path}{ext}"));
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;

    fn make_file(dir: &std::path::Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    fn make_dir(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn build_tree_raw_returns_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), "README.md", "# Hello");
        make_file(tmp.path(), "notes.txt", "ignored");
        let ignored = HashSet::new();
        let nodes = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_type, NodeType::File);
    }

    #[test]
    fn build_tree_raw_ignores_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        make_file(tmp.path(), ".hidden.md", "# Hidden");
        make_file(tmp.path(), "visible.md", "# Visible");
        let ignored = HashSet::new();
        let nodes = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "Visible");
    }

    #[test]
    fn extract_title_prefers_frontmatter_name() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("agent.md");
        fs::write(&f, "---\nname: My Agent\n---\n# Other Heading\n").unwrap();
        assert_eq!(extract_title(&f), Some("My Agent".to_string()));
    }

    #[test]
    fn extract_title_falls_back_to_h1() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("doc.md");
        fs::write(&f, "# The Title\nSome content.").unwrap();
        assert_eq!(extract_title(&f), Some("The Title".to_string()));
    }

    #[test]
    fn extract_title_returns_none_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("empty.md");
        fs::write(&f, "").unwrap();
        assert_eq!(extract_title(&f), None);
    }

    #[test]
    fn compact_tree_single_file_dir_collapses() {
        // guides/ has only intro.md → collapse to the file itself
        let tmp = tempfile::tempdir().unwrap();
        let guides = make_dir(tmp.path(), "guides");
        make_file(&guides, "intro.md", "# Intro");
        let ignored = HashSet::new();
        let raw = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        let compacted = compact_tree(raw);
        assert_eq!(compacted.len(), 1);
        assert_eq!(compacted[0].node_type, NodeType::File);
        assert_eq!(compacted[0].name, "Intro");
    }

    #[test]
    fn compact_tree_single_child_dir_merges_name() {
        // parent/ → child/ → [file1.md, file2.md] → becomes "parent/child" dir node
        let tmp = tempfile::tempdir().unwrap();
        let child = make_dir(tmp.path(), "parent/child");
        make_file(&child, "a.md", "# A");
        make_file(&child, "b.md", "# B");
        let ignored = HashSet::new();
        let raw = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        let compacted = compact_tree(raw);
        assert_eq!(compacted.len(), 1);
        assert_eq!(compacted[0].name, "parent/child");
        assert_eq!(compacted[0].node_type, NodeType::Dir);
    }

    #[test]
    fn collect_all_files_flattens_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = make_dir(tmp.path(), "sub");
        make_file(tmp.path(), "root.md", "# Root");
        make_file(&sub, "nested.md", "# Nested");
        let ignored = HashSet::new();
        let raw = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        let files = collect_all_files(&raw);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.node_type == NodeType::File));
    }
}
