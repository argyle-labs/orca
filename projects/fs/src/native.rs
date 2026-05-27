//! Native fs implementation — text/markdown only in v1. Reuses the existing
//! doc roots registry + embedded vault that live in the `docs` crate.

#![allow(clippy::disallowed_types)] // tree helpers in `docs` still pass Value blobs through; v1 mirrors their shape

use crate::{
    FsEntry, FsNodeKind, FsRootEntry, FsSearchHit, FsSearchMatch, FsStatOutput, FsTreeNode,
};
use anyhow::{Result, anyhow};
use docs::embedded;
use docs::mcp_helpers as roots_helper;
use docs::tree::{NodeType, TreeNode};
use llm::local as local_llm;
use orca_utils::config::Config;
use orca_utils::fs::expand_tilde;
use orca_utils::markdown::to_llm_text;
use std::path::{Path, PathBuf};

const EMBEDDED_ROOT: &str = "docs";

fn to_kind(t: &NodeType) -> FsNodeKind {
    match t {
        NodeType::File => FsNodeKind::File,
        NodeType::Dir => FsNodeKind::Dir,
    }
}

fn tree_node_to_fs(n: &TreeNode) -> FsTreeNode {
    FsTreeNode {
        name: n.name.clone(),
        path: n.path.clone(),
        kind: to_kind(&n.node_type),
        order: n.order,
        children: n
            .children
            .as_ref()
            .map(|cs| cs.iter().map(tree_node_to_fs).collect()),
    }
}

fn value_to_tree_node(v: &serde_json::Value) -> Option<TreeNode> {
    serde_json::from_value(v.clone()).ok()
}

fn resolve_absolute(path: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    let pb = PathBuf::from(expanded);
    if !pb.is_absolute() {
        return Err(anyhow!(
            "path must be absolute or `~/`-prefixed when no root is given: {path}"
        ));
    }
    Ok(pb)
}

/// Resolve `(root, path)` to a filesystem location. Returns `None` for the
/// embedded vault; caller dispatches separately.
fn resolve(
    config: &Config,
    root: Option<&str>,
    path: &str,
) -> Result<Option<(PathBuf, roots_helper::DocRoot)>> {
    match root {
        Some(EMBEDDED_ROOT) => Ok(None),
        Some(name) => {
            let roots = roots_helper::doc_roots(config);
            let r = roots
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow!("unknown root: {name}"))?;
            let dir = if path.is_empty() {
                r.path.clone()
            } else {
                roots_helper::resolve_within_root(&r.path, path)?
            };
            Ok(Some((dir, r)))
        }
        None => {
            let dir = resolve_absolute(path)?;
            let r = roots_helper::DocRoot {
                name: String::new(),
                path: dir.clone(),
                ignored: Default::default(),
            };
            Ok(Some((dir, r)))
        }
    }
}

pub async fn roots_list(config: &Config) -> Result<Vec<FsRootEntry>> {
    let roots = roots_helper::doc_roots(config);
    let mut out: Vec<FsRootEntry> = roots
        .iter()
        .map(|r| {
            let exists = r.path.exists();
            let count = if exists {
                roots_helper::count_doc_files(&roots_helper::build_doc_tree(
                    &r.path, &r.path, &r.ignored,
                ))
            } else {
                0
            };
            FsRootEntry {
                name: r.name.clone(),
                path: r.path.to_string_lossy().into_owned(),
                exists,
                file_count: count as u32,
            }
        })
        .collect();
    out.push(FsRootEntry {
        name: EMBEDDED_ROOT.to_string(),
        path: "(embedded in binary)".to_string(),
        exists: true,
        file_count: embedded::file_count() as u32,
    });
    Ok(out)
}

pub async fn list(config: &Config, root: Option<&str>, path: &str) -> Result<Vec<FsEntry>> {
    if matches!(root, Some(EMBEDDED_ROOT)) {
        let tree_value = embedded::tree();
        let arr = tree_value.as_array().cloned().unwrap_or_default();
        let nodes: Vec<TreeNode> = arr.iter().filter_map(value_to_tree_node).collect();
        return Ok(nodes
            .into_iter()
            .map(|n| FsEntry {
                name: n.name,
                path: n.path,
                kind: to_kind(&n.node_type),
                size: None,
            })
            .collect());
    }

    let (dir, _) = resolve(config, root, path)?.expect("non-embedded path returned");
    let mut entries: Vec<FsEntry> = Vec::new();
    for dent in std::fs::read_dir(&dir)? {
        let dent = dent?;
        let meta = dent.metadata()?;
        let name = dent.file_name().to_string_lossy().into_owned();
        entries.push(FsEntry {
            path: dent.path().to_string_lossy().into_owned(),
            kind: if meta.is_dir() {
                FsNodeKind::Dir
            } else {
                FsNodeKind::File
            },
            size: if meta.is_file() {
                Some(meta.len())
            } else {
                None
            },
            name,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

pub async fn tree(
    config: &Config,
    root: Option<&str>,
    path: &str,
    raw: bool,
) -> Result<Vec<FsTreeNode>> {
    if matches!(root, Some(EMBEDDED_ROOT)) {
        let tree_value = embedded::tree();
        let arr = tree_value.as_array().cloned().unwrap_or_default();
        return Ok(arr
            .iter()
            .filter_map(value_to_tree_node)
            .map(|n| tree_node_to_fs(&n))
            .collect());
    }

    let (dir, r) = resolve(config, root, path)?.expect("non-embedded path returned");
    let raw_nodes = roots_helper::build_doc_tree(&dir, &r.path, &r.ignored);
    let nodes = if raw {
        raw_nodes
    } else {
        roots_helper::compact_doc_tree(raw_nodes)
    };
    Ok(nodes
        .iter()
        .filter_map(value_to_tree_node)
        .map(|n| tree_node_to_fs(&n))
        .collect())
}

pub async fn read(
    config: &Config,
    root: Option<&str>,
    path: &str,
    llm_format: bool,
) -> Result<String> {
    let apply = |s: String| if llm_format { to_llm_text(&s) } else { s };

    if matches!(root, Some(EMBEDDED_ROOT)) {
        return embedded::read(path)
            .map(apply)
            .ok_or_else(|| anyhow!("not found: {EMBEDDED_ROOT}/{path}"));
    }

    match root {
        Some(name) => {
            let roots = roots_helper::doc_roots(config);
            let r = roots
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow!("unknown root: {name}"))?;
            let full = roots_helper::resolve_doc_file(&r.path, path)
                .or_else(|| roots_helper::resolve_within_root(&r.path, path).ok())
                .filter(|p: &PathBuf| p.is_file())
                .ok_or_else(|| anyhow!("not found: {name}/{path}"))?;
            Ok(apply(std::fs::read_to_string(full)?))
        }
        None => {
            let full = resolve_absolute(path)?;
            Ok(apply(std::fs::read_to_string(full)?))
        }
    }
}

pub async fn search(
    config: &Config,
    query: &str,
    filter: &str,
    llm_format: bool,
) -> Result<(Vec<FsSearchHit>, Option<String>)> {
    let all_roots = roots_helper::doc_roots(config);
    let roots: Vec<&roots_helper::DocRoot> = all_roots
        .iter()
        .filter(|r| filter == "all" || r.name == filter)
        .collect();
    let query_lower = query.to_lowercase();
    let mut hits: Vec<FsSearchHit> = Vec::new();

    for r in roots {
        if !r.path.exists() {
            continue;
        }
        let files = roots_helper::collect_all_doc_files(&roots_helper::build_doc_tree(
            &r.path, &r.path, &r.ignored,
        ));
        for file in files {
            let rel = file["path"].as_str().unwrap_or("").to_string();
            let full = r.path.join(&rel);
            let Ok(content) = std::fs::read_to_string(&full) else {
                continue;
            };
            let matches: Vec<FsSearchMatch> = content
                .lines()
                .enumerate()
                .filter(|(_, l)| l.to_lowercase().contains(&query_lower))
                .take(5)
                .map(|(i, l)| FsSearchMatch {
                    line: (i + 1) as u32,
                    text: if llm_format {
                        to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                    } else {
                        l.trim().to_string()
                    },
                })
                .collect();
            if !matches.is_empty() {
                hits.push(FsSearchHit {
                    root: r.name.clone(),
                    path: rel,
                    matches,
                });
            }
        }
    }

    if filter == "all" || filter == EMBEDDED_ROOT {
        for (path, line_matches) in embedded::search(query) {
            let matches: Vec<FsSearchMatch> = line_matches
                .into_iter()
                .enumerate()
                .map(|(i, l)| FsSearchMatch {
                    line: (i + 1) as u32,
                    text: if llm_format {
                        to_llm_text(l.trim()).trim_end_matches('\n').to_string()
                    } else {
                        l
                    },
                })
                .collect();
            hits.push(FsSearchHit {
                root: EMBEDDED_ROOT.to_string(),
                path,
                matches,
            });
        }
    }

    let summary = if !hits.is_empty()
        && let Some(llm) = local_llm::discover_local_llm().await
    {
        let raw = hits
            .iter()
            .map(|h| {
                let lines: Vec<String> = h
                    .matches
                    .iter()
                    .map(|m| format!("L{}: {}", m.line, m.text))
                    .collect();
                format!("{}/{}\n{}", h.root, h.path, lines.join("\n"))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        local_llm::present_text_results(&llm, query, &raw, 8000).await
    } else {
        None
    };

    Ok((hits, summary))
}

pub async fn stat(config: &Config, root: Option<&str>, path: &str) -> Result<FsStatOutput> {
    if matches!(root, Some(EMBEDDED_ROOT)) {
        let exists = embedded::read(path).is_some();
        return Ok(FsStatOutput {
            name: Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: path.to_string(),
            kind: FsNodeKind::File,
            size: 0,
            exists,
        });
    }

    let (full, _) = resolve(config, root, path)?.expect("non-embedded path returned");
    let exists = full.exists();
    let (kind, size) = if exists {
        let meta = std::fs::metadata(&full)?;
        let kind = if meta.is_dir() {
            FsNodeKind::Dir
        } else {
            FsNodeKind::File
        };
        (kind, meta.len())
    } else {
        (FsNodeKind::File, 0)
    };
    Ok(FsStatOutput {
        name: full
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: full.to_string_lossy().into_owned(),
        kind,
        size,
        exists,
    })
}
