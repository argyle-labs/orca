// Embedded repo documentation — compiled into the binary from docs/ at build time.
// Separate from ~/.orca/memory (per-user notes); these are project-level WHY docs
// embedded into the binary at build time.
// Accessible via root="docs" in all tree/read/search endpoints and MCP tools.
// HashMap/Value used for doc metadata blobs; protocol-level passthrough.
#![allow(clippy::disallowed_types)]

use serde_json::{Value, json};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../docs"]
struct OrcaDocs;

pub fn list() -> Vec<String> {
    let mut files: Vec<String> = OrcaDocs::iter()
        .filter(|f| f.ends_with(".md"))
        .map(|f| f.into_owned())
        .collect();
    files.sort();
    files
}

pub fn read(path: &str) -> Option<String> {
    let with_ext = if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    };
    OrcaDocs::get(&with_ext).map(|f| String::from_utf8_lossy(&f.data).into_owned())
}

pub fn search(query: &str) -> Vec<(String, Vec<String>)> {
    let q = query.to_lowercase();
    let mut results = Vec::new();
    for name in OrcaDocs::iter() {
        if !name.ends_with(".md") {
            continue;
        }
        if let Some(file) = OrcaDocs::get(&name) {
            let content = String::from_utf8_lossy(&file.data);
            let matches: Vec<String> = content
                .lines()
                .enumerate()
                .filter(|(_, l)| l.to_lowercase().contains(&q))
                .take(5)
                .map(|(i, l)| format!("L{}: {}", i + 1, l.trim()))
                .collect();
            if !matches.is_empty() {
                results.push((name.into_owned(), matches));
            }
        }
    }
    results
}

fn doc_title(path: &str) -> String {
    OrcaDocs::get(path)
        .and_then(|f| {
            let content = String::from_utf8_lossy(&f.data);
            content
                .lines()
                .find(|l| l.starts_with("# "))
                .map(|l| l[2..].trim().to_string())
        })
        .unwrap_or_else(|| {
            let stem = path
                .rsplit('/')
                .next()
                .unwrap_or(path)
                .trim_end_matches(".md");
            stem.replace('-', " ")
        })
}

pub fn tree() -> Value {
    use std::collections::BTreeMap;

    let mut top_files: Vec<Value> = Vec::new();
    let mut dirs: BTreeMap<String, Vec<Value>> = BTreeMap::new();

    for path in list() {
        match path.splitn(2, '/').collect::<Vec<_>>().as_slice() {
            [dir, _] if path.contains('/') => {
                let dir = dir.to_string();
                dirs.entry(dir).or_default().push(json!({
                    "name": doc_title(&path),
                    "path": path,
                    "type": "file"
                }));
            }
            _ => {
                top_files.push(json!({
                    "name": doc_title(&path),
                    "path": path,
                    "type": "file"
                }));
            }
        }
    }

    let mut nodes = top_files;
    for (dir_name, children) in dirs {
        nodes.push(json!({
            "name": dir_name,
            "path": dir_name,
            "type": "dir",
            "children": children
        }));
    }
    json!(nodes)
}

pub fn file_count() -> usize {
    list().len()
}

/// Typed mirror of [`tree`] returning `crate::tree::TreeNode` directly.
pub fn tree_typed() -> Vec<crate::tree::TreeNode> {
    use crate::tree::{NodeType, TreeNode};
    use std::collections::BTreeMap;

    let mut top_files: Vec<TreeNode> = Vec::new();
    let mut dirs: BTreeMap<String, Vec<TreeNode>> = BTreeMap::new();

    for path in list() {
        match path.splitn(2, '/').collect::<Vec<_>>().as_slice() {
            [dir, _] if path.contains('/') => {
                let dir = dir.to_string();
                dirs.entry(dir).or_default().push(TreeNode {
                    name: doc_title(&path),
                    path: path.clone(),
                    node_type: NodeType::File,
                    order: None,
                    children: None,
                });
            }
            _ => {
                top_files.push(TreeNode {
                    name: doc_title(&path),
                    path: path.clone(),
                    node_type: NodeType::File,
                    order: None,
                    children: None,
                });
            }
        }
    }

    let mut nodes = top_files;
    for (dir_name, children) in dirs {
        nodes.push(TreeNode {
            name: dir_name.clone(),
            path: dir_name,
            node_type: NodeType::Dir,
            order: None,
            children: Some(children),
        });
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── list ──────────────────────────────────────────────────────────────────

    #[test]
    fn list_returns_nonempty_sorted_md_files() {
        let files = list();
        assert!(!files.is_empty(), "embedded docs should not be empty");
        // All entries should end in .md
        for f in &files {
            assert!(f.ends_with(".md"), "unexpected non-.md entry: {f}");
        }
        // Should be sorted
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted, "list() should return files in sorted order");
    }

    // ── file_count ────────────────────────────────────────────────────────────

    #[test]
    fn file_count_matches_list_len() {
        assert_eq!(file_count(), list().len());
    }

    // ── read ──────────────────────────────────────────────────────────────────

    #[test]
    fn read_known_doc_returns_some() {
        // Pick the first file from list() — guaranteed to exist.
        let first = list().into_iter().next().expect("list should be nonempty");
        let content = read(&first);
        assert!(content.is_some(), "read({first}) should return Some");
        assert!(!content.unwrap().is_empty());
    }

    #[test]
    fn read_without_md_extension_also_works() {
        let first = list().into_iter().next().expect("list should be nonempty");
        let without_ext = first.trim_end_matches(".md");
        let with_ext = read(&first);
        let without = read(without_ext);
        assert_eq!(
            with_ext, without,
            "read with and without .md should return same content"
        );
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let result = read("does-not-exist-xyz");
        assert!(result.is_none());
    }

    // ── search ────────────────────────────────────────────────────────────────

    #[test]
    fn search_finds_matches_in_docs() {
        // "orca" appears in virtually every doc — should always match something.
        let results = search("orca");
        assert!(!results.is_empty(), "search for 'orca' should find results");
        // Each result has a filename and at least one matching line snippet.
        for (name, lines) in &results {
            assert!(name.ends_with(".md"), "result name should be .md: {name}");
            assert!(
                !lines.is_empty(),
                "result should have matching lines: {name}"
            );
        }
    }

    #[test]
    fn search_is_case_insensitive() {
        let lower = search("orca");
        let upper = search("ORCA");
        // Both should find at least one result
        assert!(!lower.is_empty());
        assert!(!upper.is_empty());
    }

    #[test]
    fn search_no_match_returns_empty() {
        let results = search("zzz_no_such_term_in_any_doc_xyz_999");
        assert!(
            results.is_empty(),
            "search for nonexistent term should return empty"
        );
    }

    // ── tree ──────────────────────────────────────────────────────────────────

    #[test]
    fn tree_returns_array() {
        let t = tree();
        assert!(t.is_array(), "tree() should return a JSON array");
        let arr = t.as_array().unwrap();
        assert!(!arr.is_empty(), "tree array should not be empty");
    }

    #[test]
    fn tree_nodes_have_required_fields() {
        let t = tree();
        for node in t.as_array().unwrap() {
            assert!(node["name"].is_string(), "node missing name: {node}");
            assert!(node["type"].is_string(), "node missing type: {node}");
            let ty = node["type"].as_str().unwrap();
            assert!(ty == "file" || ty == "dir", "unknown type: {ty}");
        }
    }
}
