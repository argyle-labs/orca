//! Filesystem roots — named path aliases registered via `db::docs`.
//! Moved from `namespace::file_roots`; legacy `Value`-returning tool fns
//! (`list_roots`/`get_tree`/`read_doc`/`search_docs`) were dead code and dropped.

use crate::ops::expand_tilde;
use crate::tree::{NodeType, TreeNode, build_tree_raw};
use anyhow::Result;
use contract::config::Config;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct FileRoot {
    pub name: String,
    pub path: PathBuf,
    pub ignored: HashSet<String>,
}

pub fn file_roots(_config: &Config) -> Vec<FileRoot> {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let patterns: HashSet<String> = db::docs::list_ignore_patterns(&conn)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let rows = db::docs::list_roots(&conn).unwrap_or_default();
    rows.into_iter()
        .map(|r| FileRoot {
            name: r.name,
            path: PathBuf::from(expand_tilde(&r.path)),
            ignored: patterns.clone(),
        })
        .collect()
}

pub fn build_doc_tree(dir: &Path, root_dir: &Path, ignored: &HashSet<String>) -> Vec<TreeNode> {
    build_tree_raw(dir, root_dir, ignored)
}

pub fn count_doc_files(nodes: &[TreeNode]) -> usize {
    nodes
        .iter()
        .map(|n| match n.node_type {
            NodeType::File => 1,
            NodeType::Dir => n.children.as_ref().map(|c| count_doc_files(c)).unwrap_or(0),
        })
        .sum()
}

fn find_single_doc_file(nodes: &[TreeNode]) -> Option<TreeNode> {
    for node in nodes {
        if node.node_type == NodeType::File {
            return Some(node.clone());
        }
        if let Some(children) = node.children.as_ref()
            && let Some(found) = find_single_doc_file(children)
        {
            return Some(found);
        }
    }
    None
}

pub fn compact_doc_tree(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    let mut result = vec![];
    for node in nodes {
        if node.node_type == NodeType::File {
            result.push(node);
            continue;
        }

        let children_raw = node.children.clone().unwrap_or_default();
        let children = compact_doc_tree(children_raw);

        if count_doc_files(&children) == 1
            && let Some(file) = find_single_doc_file(&children)
        {
            result.push(file);
            continue;
        }

        if children.len() == 1 && children[0].node_type == NodeType::Dir {
            let child = &children[0];
            let merged = format!("{}/{}", node.name, child.name);
            let mut n = child.clone();
            n.name = merged;
            result.push(n);
            continue;
        }

        let mut n = node.clone();
        n.children = Some(children);
        result.push(n);
    }
    result
}

pub fn collect_all_doc_files(nodes: &[TreeNode]) -> Vec<TreeNode> {
    let mut files = vec![];
    for node in nodes {
        if node.node_type == NodeType::File {
            files.push(node.clone());
        } else if let Some(children) = node.children.as_ref() {
            files.extend(collect_all_doc_files(children));
        }
    }
    files
}

/// Resolve `rel` relative to `root`, verifying the result stays within `root`.
pub fn resolve_within_root(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = root.join(rel);
    let canonical = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.clone());
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !canonical.starts_with(&root_canonical) {
        anyhow::bail!("path escapes root: {rel}");
    }
    Ok(canonical)
}

pub fn resolve_doc_file(root_dir: &Path, doc_path: &str) -> Option<PathBuf> {
    for ext in &[".md", ".mdx", ""] {
        let rel = format!("{doc_path}{ext}");
        if let Ok(full) = resolve_within_root(root_dir, &rel)
            && full.is_file()
        {
            return Some(full);
        }
    }
    None
}
