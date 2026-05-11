use crate::commands::list_embedded_commands;
use anyhow::Result;
use orca_utils::config::Config;
use orca_utils::fs::expand_tilde;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::markdown::to_llm_text;
use crate::serve::tree::{TreeNode, build_tree_raw};

pub struct DocRoot {
    pub name: String,
    pub path: PathBuf,
    pub ignored: HashSet<String>,
}

pub fn doc_roots(_config: &Config) -> Vec<DocRoot> {
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
        .map(|r| DocRoot {
            name: r.name,
            path: PathBuf::from(expand_tilde(&r.path)),
            ignored: patterns.clone(),
        })
        .collect()
}

pub fn build_doc_tree(dir: &Path, root_dir: &Path, ignored: &HashSet<String>) -> Vec<Value> {
    tree_nodes_to_values(build_tree_raw(dir, root_dir, ignored))
}

fn tree_nodes_to_values(nodes: Vec<TreeNode>) -> Vec<Value> {
    nodes
        .into_iter()
        .filter_map(|n| serde_json::to_value(n).ok())
        .collect()
}

pub fn count_doc_files(nodes: &[Value]) -> usize {
    nodes
        .iter()
        .map(|n| {
            if n["type"] == "file" {
                1
            } else {
                n["children"]
                    .as_array()
                    .map(|c| count_doc_files(c))
                    .unwrap_or(0)
            }
        })
        .sum()
}

fn find_single_doc_file(nodes: &[Value]) -> Option<Value> {
    for node in nodes {
        if node["type"] == "file" {
            return Some(node.clone());
        }
        if let Some(children) = node["children"].as_array()
            && let Some(found) = find_single_doc_file(children)
        {
            return Some(found);
        }
    }
    None
}

pub fn compact_doc_tree(nodes: Vec<Value>) -> Vec<Value> {
    let mut result = vec![];
    for node in nodes {
        if node["type"] == "file" {
            result.push(node);
            continue;
        }

        let children_raw: Vec<Value> = node["children"].as_array().cloned().unwrap_or_default();
        let children = compact_doc_tree(children_raw);

        if count_doc_files(&children) == 1
            && let Some(file) = find_single_doc_file(&children)
        {
            result.push(file);
            continue;
        }

        if children.len() == 1 && children[0]["type"] == "dir" {
            let child = &children[0];
            let merged = format!(
                "{}/{}",
                node["name"].as_str().unwrap_or(""),
                child["name"].as_str().unwrap_or("")
            );
            let mut n = child.clone();
            n["name"] = json!(merged);
            result.push(n);
            continue;
        }

        let mut n = node.clone();
        n["children"] = json!(children);
        result.push(n);
    }
    result
}

pub fn collect_all_doc_files(nodes: &[Value]) -> Vec<Value> {
    let mut files = vec![];
    for node in nodes {
        if node["type"] == "file" {
            files.push(node.clone());
        } else if let Some(children) = node["children"].as_array() {
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

// ── Tool implementations ──────────────────────────────────────────────────────

pub fn list_roots(config: &Config) -> Result<String> {
    let roots = doc_roots(config);
    let mut entries: Vec<Value> = roots
        .iter()
        .map(|r| {
            let exists = r.path.exists();
            let docs = if exists {
                count_doc_files(&build_doc_tree(&r.path, &r.path, &r.ignored))
            } else {
                0
            };
            json!({ "root": r.name, "path": r.path.to_string_lossy(), "exists": exists, "docs": docs })
        })
        .collect();
    entries.push(json!({
        "root": "docs",
        "path": "(embedded in binary)",
        "exists": true,
        "docs": crate::docs::file_count()
    }));
    Ok(serde_json::to_string_pretty(&entries)?)
}

pub fn get_tree(args: &Value, config: &Config) -> Result<String> {
    let root_name = args["root"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("root is required"))?;

    if root_name == "docs" {
        return Ok(serde_json::to_string_pretty(&crate::docs::tree())?);
    }

    let sub_path = args["path"].as_str();
    let roots = doc_roots(config);
    let root = roots
        .iter()
        .find(|r| r.name == root_name)
        .ok_or_else(|| anyhow::anyhow!("unknown root: {root_name}"))?;

    let dir = match sub_path {
        Some(p) => resolve_within_root(&root.path, p)?,
        None => root.path.clone(),
    };
    let compact = compact_doc_tree(build_doc_tree(&dir, &root.path, &root.ignored));
    Ok(serde_json::to_string_pretty(&compact)?)
}

pub fn read_doc(args: &Value, config: &Config) -> Result<String> {
    let root_name = args["root"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("root is required"))?;
    let doc_path = args["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("path is required"))?;
    let llm_mode = args["format"].as_str() == Some("llm");

    let apply = |s: String| if llm_mode { to_llm_text(&s) } else { s };

    if root_name == "docs" {
        return crate::docs::read(doc_path)
            .map(apply)
            .ok_or_else(|| anyhow::anyhow!("not found: docs/{doc_path}"));
    }

    let roots = doc_roots(config);
    let root = roots
        .iter()
        .find(|r| r.name == root_name)
        .ok_or_else(|| anyhow::anyhow!("unknown root: {root_name}"))?;

    let full = resolve_doc_file(&root.path, doc_path)
        .ok_or_else(|| anyhow::anyhow!("not found: {root_name}/{doc_path}"))?;

    Ok(apply(std::fs::read_to_string(full)?))
}

pub fn search_docs(args: &Value, config: &Config) -> Result<String> {
    let query = args["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query is required"))?;
    let filter = args["root"].as_str().unwrap_or("all");
    let llm_mode = args["format"].as_str() == Some("llm");

    let all_roots = doc_roots(config);
    let roots: Vec<&DocRoot> = all_roots
        .iter()
        .filter(|r| filter == "all" || r.name == filter)
        .collect();

    let query_lower = query.to_lowercase();
    let mut results: Vec<String> = vec![];

    for root in roots {
        if !root.path.exists() {
            continue;
        }
        let files = collect_all_doc_files(&build_doc_tree(&root.path, &root.path, &root.ignored));
        for file in files {
            let rel = file["path"].as_str().unwrap_or("");
            let full = root.path.join(rel);
            let Ok(content) = std::fs::read_to_string(&full) else {
                continue;
            };
            let matches: Vec<String> = content
                .lines()
                .enumerate()
                .filter(|(_, l)| l.to_lowercase().contains(&query_lower))
                .take(5)
                .map(|(i, l)| {
                    let line = if llm_mode {
                        to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                    } else {
                        l.trim().to_string()
                    };
                    format!("L{}: {}", i + 1, line)
                })
                .collect();
            if !matches.is_empty() {
                results.push(format!("{}/{}\n{}", root.name, rel, matches.join("\n")));
            }
        }
    }

    if filter == "all" || filter == "docs" {
        for (path, matches) in crate::docs::search(query) {
            let matches: Vec<String> = matches
                .into_iter()
                .map(|l| {
                    if llm_mode {
                        to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                    } else {
                        l
                    }
                })
                .collect();
            results.push(format!("docs/{}\n{}", path, matches.join("\n")));
        }
    }

    if results.is_empty() {
        Ok(format!("No results for \"{query}\""))
    } else {
        Ok(results.join("\n\n"))
    }
}

pub fn list_commands(_config: &Config) -> Result<String> {
    let names = list_embedded_commands();
    if names.is_empty() {
        return Ok("No commands embedded.".into());
    }
    Ok(names.join("\n"))
}
