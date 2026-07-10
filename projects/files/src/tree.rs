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
    pub order: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
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
        "orca".to_string(),
        PathBuf::from(
            std::env::var("ORCA_CODE_ROOT")
                .unwrap_or_else(|_| format!("{home}/code/argyle-labs/orca")),
        ),
    );
    roots.insert(
        "dotfiles".to_string(),
        PathBuf::from(
            std::env::var("DOTFILES_ROOT").unwrap_or_else(|_| format!("{home}/dotfiles")),
        ),
    );
    roots
}

pub fn get_ignored(root_name: &str) -> HashSet<String> {
    match root_name {
        "orca" => [".git", "target", "node_modules", "dist", "build", ".next"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        "dotfiles" => HashSet::new(),
        _ => HashSet::new(),
    }
}

// Search intentionally includes memory/ (unlike the nav tree) so Claude can
// find relevant context across past decisions without exposing the raw tree.
pub fn get_search_ignored(root_name: &str) -> HashSet<String> {
    match root_name {
        "orca" => [".git", "logs", ".trash", "node_modules", "plugins"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        _ => HashSet::new(),
    }
}

fn parse_numeric_prefix(name: &str) -> (Option<u32>, String) {
    if let Some((prefix, rest)) = name.split_once('-')
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_digit())
    {
        let order = prefix.parse::<u32>().ok();
        return (order, rest.to_string());
    }
    (None, name.to_string())
}

fn strip_app_prefix(title: &str) -> String {
    // Strip "AppName — " prefix from titles like "my-cli — Patterns"
    if let Some((_, rest)) = title.split_once(" \u{2014} ") {
        rest.trim().to_string()
    } else {
        title.to_string()
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
        .map(|l| strip_app_prefix(l[2..].trim()))
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
                let (order, stripped) = parse_numeric_prefix(&name);
                let display_name = match order {
                    Some(n) => format!("{n}. {stripped}"),
                    None => stripped,
                };
                nodes.push(TreeNode {
                    name: display_name,
                    path: rel,
                    order,
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
                let (order, stem_stripped) = parse_numeric_prefix(&stem);
                let base_title = extract_title(&full).unwrap_or(stem_stripped);
                let title = match order {
                    Some(n) => format!("{n}. {base_title}"),
                    None => base_title,
                };
                nodes.push(TreeNode {
                    name: title,
                    path: rel,
                    order,
                    node_type: NodeType::File,
                    children: None,
                });
            }
        }
    }
    nodes.sort_by(|a, b| match (a.order, b.order) {
        (Some(oa), Some(ob)) => oa.cmp(&ob),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.path.cmp(&b.path),
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
            let child = children.into_iter().next().expect("checked len == 1");
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

/// Returns the tree without compaction — directory structure matches the filesystem exactly.
pub fn get_root_tree_raw(root_name: &str) -> Vec<TreeNode> {
    let roots = get_roots();
    let Some(root_dir) = roots.get(root_name) else {
        return vec![];
    };
    let ignored = get_ignored(root_name);
    let root_dir = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());
    build_tree_raw(&root_dir, &root_dir, &ignored)
}

pub fn get_root_tree(root_name: &str) -> Vec<TreeNode> {
    let roots = get_roots();
    let Some(root_dir) = roots.get(root_name) else {
        return vec![];
    };
    let ignored = get_ignored(root_name);
    // Canonicalize so strip_prefix works correctly when root_dir is a symlink
    let root_dir = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());
    compact_tree(build_tree_raw(&root_dir, &root_dir, &ignored))
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

    #[test]
    fn raw_tree_preserves_single_file_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = make_dir(tmp.path(), "guides");
        make_file(&sub, "intro.md", "# Intro");
        let ignored = HashSet::new();
        let raw = build_tree_raw(tmp.path(), tmp.path(), &ignored);
        // raw should keep guides/ as a dir, not collapse it
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].node_type, NodeType::Dir);
        assert_eq!(raw[0].name, "guides");
    }

    #[test]
    fn extract_title_strips_app_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("doc.md");
        fs::write(
            &f,
            "# my-cli \u{2014} Patterns: Idioms and Conventions\nContent.",
        )
        .unwrap();
        assert_eq!(
            extract_title(&f),
            Some("Patterns: Idioms and Conventions".to_string())
        );
    }

    #[test]
    fn extract_title_no_prefix_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("doc.md");
        fs::write(&f, "# Just A Title\nContent.").unwrap();
        assert_eq!(extract_title(&f), Some("Just A Title".to_string()));
    }
}
