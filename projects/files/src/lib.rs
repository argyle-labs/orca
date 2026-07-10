//! `files` — unified filesystem primitives + `files.*` `#[orca_tool]` surface.
//!
//! Consolidated from `utils::{fs,embedded,tree,fs_native,fs_tools}` and
//! `namespace::file_roots` (slice: fs consolidation, 2026-05-29). Renamed
//! from `fs` to `files` to free up `std::fs` collision and reflect the
//! domain (typed file/root operations) rather than a primitive.

pub mod embedded;
pub mod markdown;
pub mod ops;
pub mod roots;
pub mod tools;
pub mod tree;
pub mod watch;

use crate::embedded::file_count as embedded_file_count;
use crate::markdown::to_llm_text;
use crate::ops::expand_tilde;
use crate::tools::{
    FsEntry, FsNodeKind, FsRootEntry, FsSearchHit, FsSearchMatch, FsStatOutput, FsTreeNode,
};
use crate::tree::{NodeType, TreeNode};
use anyhow::{Result, anyhow};
use contract::config::Config;
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

fn resolve(
    config: &Config,
    root: Option<&str>,
    path: &str,
) -> Result<Option<(PathBuf, roots::FileRoot)>> {
    match root {
        Some(EMBEDDED_ROOT) => Ok(None),
        Some(name) => {
            let rs = roots::file_roots(config);
            let r = rs
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow!("unknown root: {name}"))?;
            let dir = if path.is_empty() {
                r.path.clone()
            } else {
                roots::resolve_within_root(&r.path, path)?
            };
            Ok(Some((dir, r)))
        }
        None => {
            let dir = resolve_absolute(path)?;
            let r = roots::FileRoot {
                name: String::new(),
                path: dir.clone(),
                ignored: Default::default(),
            };
            Ok(Some((dir, r)))
        }
    }
}

pub async fn roots_list(config: &Config) -> Result<Vec<FsRootEntry>> {
    let rs = roots::file_roots(config);
    let mut out: Vec<FsRootEntry> = rs
        .iter()
        .map(|r| {
            let exists = r.path.exists();
            let count = if exists {
                roots::count_doc_files(&roots::build_doc_tree(&r.path, &r.path, &r.ignored))
            } else {
                0
            };
            FsRootEntry {
                name: r.name.clone(),
                path: r.path.to_string_lossy().into_owned(),
                description: None,
                enabled: true,
                exists,
                file_count: count as u32,
            }
        })
        .collect();
    out.push(FsRootEntry {
        name: EMBEDDED_ROOT.to_string(),
        path: "(embedded in binary)".to_string(),
        description: Some("embedded in binary".to_string()),
        enabled: true,
        exists: true,
        file_count: embedded_file_count() as u32,
    });
    Ok(out)
}

pub async fn list(config: &Config, root: Option<&str>, path: &str) -> Result<Vec<FsEntry>> {
    if matches!(root, Some(EMBEDDED_ROOT)) {
        let nodes = embedded::tree_typed();
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
        let nodes = embedded::tree_typed();
        return Ok(nodes.iter().map(tree_node_to_fs).collect());
    }

    let (dir, r) = resolve(config, root, path)?.expect("non-embedded path returned");
    let raw_nodes = roots::build_doc_tree(&dir, &r.path, &r.ignored);
    let nodes = if raw {
        raw_nodes
    } else {
        roots::compact_doc_tree(raw_nodes)
    };
    Ok(nodes.iter().map(tree_node_to_fs).collect())
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
            let rs = roots::file_roots(config);
            let r = rs
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow!("unknown root: {name}"))?;
            let full = roots::resolve_doc_file(&r.path, path)
                .or_else(|| roots::resolve_within_root(&r.path, path).ok())
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

/// Case-insensitive line search across one or all registered roots. Hits-only —
/// LLM summary surface dropped 2026-05-29 (callers can format hits themselves).
pub async fn search(config: &Config, query: &str, filter: &str) -> Result<Vec<FsSearchHit>> {
    let all_roots = roots::file_roots(config);
    let rs: Vec<&roots::FileRoot> = all_roots
        .iter()
        .filter(|r| filter == "all" || r.name == filter)
        .collect();
    let query_lower = query.to_lowercase();
    let mut hits: Vec<FsSearchHit> = Vec::new();

    for r in rs {
        if !r.path.exists() {
            continue;
        }
        let files =
            roots::collect_all_doc_files(&roots::build_doc_tree(&r.path, &r.path, &r.ignored));
        for file in files {
            let rel = file.path.clone();
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
                    text: l.trim().to_string(),
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
                    text: l,
                })
                .collect();
            hits.push(FsSearchHit {
                root: EMBEDDED_ROOT.to_string(),
                path,
                matches,
            });
        }
    }

    Ok(hits)
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
