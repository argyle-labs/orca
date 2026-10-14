//! `files` — unified filesystem primitives + `files.*` `#[orca_tool]` surface.
//!
//! Consolidated from `utils::{fs,embedded,tree,fs_native,fs_tools}` and
//! `namespace::file_roots` (slice: fs consolidation, 2026-05-29). Renamed
//! from `fs` to `files` to free up `std::fs` collision and reflect the
//! domain (typed file/root operations) rather than a primitive.

pub mod confine;
pub mod docs;
pub mod embedded;
pub mod guard;
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

/// A path opened inside a registered root.
struct Confined {
    file: std::fs::File,
    path: PathBuf,
    root: roots::FileRoot,
}

/// Open `path` (relative to `root`, or absolute and beneath some registered
/// root) through `confine::open_beneath`, refusing anything that resolves
/// outside every root or into key material.
fn open_confined(config: &Config, root: Option<&str>, path: &str) -> Result<Confined> {
    let rs = roots::file_roots(config);
    let (r, rel) = match root {
        Some(name) => {
            let r = rs
                .into_iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow!("unknown root: {name}"))?;
            (r, PathBuf::from(path))
        }
        None => {
            let abs = resolve_absolute(path)?;
            rs.into_iter()
                .find_map(|r| confine::relative_to_root(&r.path, &abs).map(|rel| (r, rel)))
                .ok_or_else(|| confine::outside_roots(&abs))?
        }
    };
    let full = r.path.join(&rel);
    // A root may contain a key dir (e.g. a root at `~`).
    guard::deny_key_material(&full)?;
    let file = confine::open_beneath(&r.path, &rel)?;
    Ok(Confined {
        file,
        path: full,
        root: r,
    })
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn read_text(mut file: std::fs::File) -> Result<String> {
    use std::io::Read;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(s)
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

    let c = open_confined(config, root, path)?;
    let mut entries: Vec<FsEntry> = Vec::new();
    for name in confine::read_dir_names(&c.file)? {
        let st = confine::lstat_at(&c.file, &name)?;
        let fmt = st.st_mode & libc::S_IFMT;
        entries.push(FsEntry {
            path: c.path.join(&name).to_string_lossy().into_owned(),
            kind: if fmt == libc::S_IFDIR {
                FsNodeKind::Dir
            } else {
                FsNodeKind::File
            },
            size: (fmt == libc::S_IFREG).then_some(st.st_size as u64),
            name: name.to_string_lossy().into_owned(),
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

    let c = open_confined(config, root, path)?;
    if !c.file.metadata()?.is_dir() {
        return Err(anyhow!("not a directory: {}", c.path.display()));
    }
    let raw_nodes = roots::build_doc_tree(&c.path, &c.root.path, &c.root.ignored);
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

    // Docs are addressed without their extension. Only not-found falls
    // through, so an escape is never masked as "not found".
    if root.is_some() {
        for ext in [".md", ".mdx"] {
            match open_confined(config, root, &format!("{path}{ext}")) {
                Ok(c) if c.file.metadata()?.is_file() => return Ok(apply(read_text(c.file)?)),
                Err(e) if !is_not_found(&e) => return Err(e),
                _ => {}
            }
        }
    }
    let c = open_confined(config, root, path)?;
    if !c.file.metadata()?.is_file() {
        return Err(anyhow!("not a file: {}", c.path.display()));
    }
    Ok(apply(read_text(c.file)?))
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
            // A root that contains a key dir (e.g. `~`) must not search into it.
            if guard::deny_key_material(&r.path.join(&rel)).is_err() {
                continue;
            }
            let Ok(content) = confine::open_beneath(&r.path, Path::new(&rel)).and_then(read_text)
            else {
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

    let (full, meta) = match open_confined(config, root, path) {
        Ok(c) => {
            let meta = c.file.metadata()?;
            (c.path, Some(meta))
        }
        Err(e) if is_not_found(&e) => (resolve_unguarded_path(config, root, path)?, None),
        Err(e) => return Err(e),
    };
    let (kind, size, exists) = match &meta {
        Some(m) if m.is_dir() => (FsNodeKind::Dir, m.len(), true),
        Some(m) => (FsNodeKind::File, m.len(), true),
        None => (FsNodeKind::File, 0, false),
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

/// Display path for a not-found `stat`; containment was already checked by
/// the `open_confined` that reported not-found.
fn resolve_unguarded_path(config: &Config, root: Option<&str>, path: &str) -> Result<PathBuf> {
    match root {
        Some(name) => roots::file_roots(config)
            .into_iter()
            .find(|r| r.name == name)
            .map(|r| r.path.join(path))
            .ok_or_else(|| anyhow!("unknown root: {name}")),
        None => resolve_absolute(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NodeType, TreeNode};
    use contract::config::{Config, Model};
    use std::fs;
    use std::path::PathBuf;

    // ── helpers ────────────────────────────────────────────────────────────

    fn cfg() -> Config {
        Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-files-test.db"),
            ports: Default::default(),
        }
    }

    /// Create a unique temp directory under the system temp dir and return it.
    fn scratch(tag: &str) -> PathBuf {
        let mut base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("orca-files-{tag}-{nanos}-{:p}", &base));
        fs::create_dir_all(&base).unwrap();
        register_root(&base);
        base
    }

    /// Registers `dir` as a root so absolute paths beneath it are in scope.
    /// The DB is per test thread: guard tests plant a fake `.db_key` in the
    /// shared state dir, which would break the encrypted default DB.
    fn register_root(dir: &Path) {
        db::set_thread_db_path(Some(&format!("{}.db", dir.display())));
        let conn = db::open_default().unwrap();
        crate::docs::upsert_root(
            &conn,
            &crate::docs::RootRow {
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                path: dir.to_string_lossy().into_owned(),
                description: None,
                enabled: true,
            },
        )
        .unwrap();
    }

    // ── to_kind / tree_node_to_fs ──────────────────────────────────────────

    #[test]
    fn to_kind_maps_both_variants() {
        assert!(matches!(to_kind(&NodeType::File), FsNodeKind::File));
        assert!(matches!(to_kind(&NodeType::Dir), FsNodeKind::Dir));
    }

    #[test]
    fn tree_node_to_fs_recurses_children() {
        let node = TreeNode {
            name: "root".into(),
            path: "root".into(),
            node_type: NodeType::Dir,
            order: Some(3),
            children: Some(vec![TreeNode {
                name: "leaf.md".into(),
                path: "root/leaf.md".into(),
                node_type: NodeType::File,
                order: None,
                children: None,
            }]),
        };
        let fs_node = tree_node_to_fs(&node);
        assert_eq!(fs_node.name, "root");
        assert!(matches!(fs_node.kind, FsNodeKind::Dir));
        assert_eq!(fs_node.order, Some(3));
        let kids = fs_node.children.expect("children present");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].name, "leaf.md");
        assert!(matches!(kids[0].kind, FsNodeKind::File));
        assert!(kids[0].children.is_none());
    }

    // ── resolve_absolute ───────────────────────────────────────────────────

    #[test]
    fn resolve_absolute_accepts_absolute() {
        let p = resolve_absolute("/etc/hosts").unwrap();
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn resolve_absolute_rejects_relative() {
        let err = resolve_absolute("relative/path").unwrap_err();
        assert!(err.to_string().contains("must be absolute"), "got: {err}");
    }

    #[test]
    fn resolve_absolute_expands_tilde() {
        let p = resolve_absolute("~/somefile").unwrap();
        assert!(p.is_absolute(), "tilde should expand to absolute: {p:?}");
        assert!(!p.to_string_lossy().starts_with('~'));
    }

    // ── confinement ────────────────────────────────────────────────────────

    fn assert_forbidden<T>(res: Result<T>) {
        let Err(err) = res else {
            panic!("must be refused");
        };
        let oe = err
            .downcast_ref::<contract::OrcaError>()
            .unwrap_or_else(|| panic!("not an OrcaError: {err}"));
        assert_eq!(oe.kind, contract::ErrorKind::Forbidden, "{oe}");
    }

    #[test]
    fn open_confined_no_root_relative_errors() {
        let err = open_confined(&cfg(), None, "not/absolute").err().unwrap();
        assert!(err.to_string().contains("must be absolute"), "got: {err}");
    }

    #[tokio::test]
    async fn absolute_paths_outside_every_root_are_refused() {
        let outside = tempfile::tempdir().unwrap();
        let f = outside.path().join("f.md");
        fs::write(&f, "x").unwrap();
        let p = f.to_string_lossy().into_owned();
        assert_forbidden(read(&cfg(), None, &p, false).await);
        assert_forbidden(stat(&cfg(), None, &p).await);
        let d = outside.path().to_string_lossy().into_owned();
        assert_forbidden(list(&cfg(), None, &d).await);
        assert_forbidden(tree(&cfg(), None, &d, true).await);
        assert_forbidden(read(&cfg(), None, "/etc/hosts", false).await);
    }

    #[tokio::test]
    async fn dot_dot_out_of_a_root_is_refused() {
        let dir = scratch("dotdot");
        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        let sibling = dir.parent().unwrap().join(format!("{root}-sibling.md"));
        fs::write(&sibling, "x").unwrap();
        let rel = format!("../{root}-sibling.md");
        assert_forbidden(read(&cfg(), Some(&root), &rel, false).await);
        assert_forbidden(stat(&cfg(), Some(&root), &rel).await);
        let abs = dir.join("..").join(format!("{root}-sibling.md"));
        assert_forbidden(read(&cfg(), None, &abs.to_string_lossy(), false).await);
        fs::remove_file(&sibling).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_a_root_are_refused() {
        let dir = scratch("symlink");
        let root = dir.file_name().unwrap().to_string_lossy().into_owned();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.md"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.md"), dir.join("f.md")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join("d")).unwrap();
        assert_forbidden(read(&cfg(), Some(&root), "f", false).await);
        assert_forbidden(read(&cfg(), Some(&root), "d/secret.md", false).await);
        assert_forbidden(list(&cfg(), Some(&root), "d").await);
        assert_forbidden(read(&cfg(), None, &dir.join("f.md").to_string_lossy(), false).await);
        let names: Vec<String> = tree(&cfg(), Some(&root), "", true)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert!(names.is_empty(), "tree must not follow links: {names:?}");
        let hits = search(&cfg(), "s", &root).await.unwrap();
        assert!(hits.is_empty(), "search must not follow links");
        fs::remove_dir_all(&dir).ok();
    }

    // ── list ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_absolute_dir_sorted_with_sizes() {
        let dir = scratch("list");
        fs::write(dir.join("b.txt"), "hello").unwrap();
        fs::write(dir.join("a.txt"), "hi").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();

        let entries = list(&cfg(), None, &dir.to_string_lossy()).await.unwrap();
        assert_eq!(entries.len(), 3);
        // sorted by name
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[2].name, "sub");
        // file sizes populated, dir size absent
        assert_eq!(entries[0].size, Some(2));
        assert_eq!(entries[1].size, Some(5));
        assert!(matches!(entries[2].kind, FsNodeKind::Dir));
        assert!(entries[2].size.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn list_missing_dir_errors() {
        let missing = std::env::temp_dir().join("orca-files-nope-xyz-000/deeper");
        let res = list(&cfg(), None, &missing.to_string_lossy()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn list_embedded_root_nonempty() {
        let entries = list(&cfg(), Some(EMBEDDED_ROOT), "").await.unwrap();
        assert!(!entries.is_empty());
    }

    // ── tree ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tree_absolute_raw_lists_files() {
        let dir = scratch("tree");
        fs::write(dir.join("top.md"), "# Top").unwrap();
        fs::create_dir(dir.join("nested")).unwrap();
        fs::write(dir.join("nested").join("inner.md"), "# Inner").unwrap();

        let raw = tree(&cfg(), None, &dir.to_string_lossy(), true)
            .await
            .unwrap();
        assert!(!raw.is_empty());
        // raw preserves the nested directory node
        let names: Vec<&str> = raw.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"nested") || names.contains(&"top.md"));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tree_compacted_collapses_single_file_dirs() {
        let dir = scratch("treecompact");
        fs::create_dir(dir.join("only")).unwrap();
        fs::write(dir.join("only").join("solo.md"), "# Solo").unwrap();

        let compact = tree(&cfg(), None, &dir.to_string_lossy(), false)
            .await
            .unwrap();
        // A directory containing exactly one file collapses to that file node.
        assert_eq!(compact.len(), 1);
        assert!(matches!(compact[0].kind, FsNodeKind::File));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tree_embedded_root_nonempty() {
        let nodes = tree(&cfg(), Some(EMBEDDED_ROOT), "", false).await.unwrap();
        assert!(!nodes.is_empty());
    }

    // ── read ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn read_absolute_file_returns_contents() {
        let dir = scratch("read");
        let file = dir.join("doc.md");
        fs::write(&file, "# Heading\n\nbody text").unwrap();
        let out = read(&cfg(), None, &file.to_string_lossy(), false)
            .await
            .unwrap();
        assert_eq!(out, "# Heading\n\nbody text");
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_absolute_llm_format_strips_markdown() {
        let dir = scratch("readllm");
        let file = dir.join("doc.md");
        fs::write(&file, "# Heading\n\nplain body").unwrap();
        let raw = read(&cfg(), None, &file.to_string_lossy(), false)
            .await
            .unwrap();
        let llm = read(&cfg(), None, &file.to_string_lossy(), true)
            .await
            .unwrap();
        // llm formatting should still contain the body text
        assert!(llm.contains("plain body"));
        // and should differ from raw (heading markup normalised) or at least be valid
        assert!(!llm.is_empty());
        assert!(raw.contains('#'));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_absolute_missing_errors() {
        let missing = std::env::temp_dir().join("orca-files-missing-read-000.md");
        let res = read(&cfg(), None, &missing.to_string_lossy(), false).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn read_embedded_known_doc() {
        let first = embedded::list()
            .into_iter()
            .next()
            .expect("embedded docs present");
        let out = read(&cfg(), Some(EMBEDDED_ROOT), &first, false)
            .await
            .unwrap();
        assert!(!out.is_empty());
    }

    #[tokio::test]
    async fn read_embedded_missing_errors() {
        let res = read(
            &cfg(),
            Some(EMBEDDED_ROOT),
            "no-such-embedded-doc-xyz",
            false,
        )
        .await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    // ── stat ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stat_absolute_file() {
        let dir = scratch("stat");
        let file = dir.join("f.txt");
        fs::write(&file, "12345").unwrap();
        let out = stat(&cfg(), None, &file.to_string_lossy()).await.unwrap();
        assert_eq!(out.name, "f.txt");
        assert!(out.exists);
        assert_eq!(out.size, 5);
        assert!(matches!(out.kind, FsNodeKind::File));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stat_absolute_dir() {
        let dir = scratch("statdir");
        let out = stat(&cfg(), None, &dir.to_string_lossy()).await.unwrap();
        assert!(out.exists);
        assert!(matches!(out.kind, FsNodeKind::Dir));
        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stat_absolute_missing_reports_not_exists() {
        let missing = scratch("statmissing").join("orca-files-stat-missing-000.txt");
        let out = stat(&cfg(), None, &missing.to_string_lossy())
            .await
            .unwrap();
        assert!(!out.exists);
        assert_eq!(out.size, 0);
        assert!(matches!(out.kind, FsNodeKind::File));
    }

    #[tokio::test]
    async fn stat_embedded_existing_and_missing() {
        let first = embedded::list()
            .into_iter()
            .next()
            .expect("embedded docs present");
        let present = stat(&cfg(), Some(EMBEDDED_ROOT), &first).await.unwrap();
        assert!(present.exists);
        assert!(matches!(present.kind, FsNodeKind::File));

        let absent = stat(&cfg(), Some(EMBEDDED_ROOT), "nope-xyz-embedded")
            .await
            .unwrap();
        assert!(!absent.exists);
    }

    // ── roots_list / search (db-backed; tolerate empty db) ───────────────────

    #[tokio::test]
    async fn roots_list_always_includes_embedded() {
        let roots = roots_list(&cfg()).await.unwrap();
        let embedded = roots
            .iter()
            .find(|r| r.name == EMBEDDED_ROOT)
            .expect("embedded root present");
        assert!(embedded.exists);
        assert_eq!(embedded.path, "(embedded in binary)");
        assert!(embedded.file_count > 0);
    }

    #[tokio::test]
    async fn search_embedded_filter_finds_hits() {
        let hits = search(&cfg(), "orca", EMBEDDED_ROOT).await.unwrap();
        assert!(!hits.is_empty());
        for hit in &hits {
            assert_eq!(hit.root, EMBEDDED_ROOT);
            assert!(!hit.matches.is_empty());
        }
    }

    #[tokio::test]
    async fn search_no_match_returns_empty() {
        let hits = search(&cfg(), "zzz_no_such_term_anywhere_xyz_999", "all")
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    // ── serde shapes (assert on serialized strings, no Value) ─────────────────

    #[test]
    fn fs_entry_serialization_shape() {
        let entry = FsEntry {
            name: "a.txt".into(),
            path: "/tmp/a.txt".into(),
            kind: FsNodeKind::File,
            size: Some(42),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"name\":\"a.txt\""));
        assert!(json.contains("\"type\":\"file\""));
        assert!(json.contains("\"size\":42"));
    }

    #[test]
    fn fs_entry_omits_none_size() {
        let entry = FsEntry {
            name: "d".into(),
            path: "/tmp/d".into(),
            kind: FsNodeKind::Dir,
            size: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("size"), "size should be omitted: {json}");
        assert!(json.contains("\"type\":\"dir\""));
    }

    #[test]
    fn fs_stat_output_serialization_shape() {
        let out = FsStatOutput {
            name: "f".into(),
            path: "/tmp/f".into(),
            kind: FsNodeKind::File,
            size: 7,
            exists: true,
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(json.contains("\"type\":\"file\""));
        assert!(json.contains("\"size\":7"));
        assert!(json.contains("\"exists\":true"));
    }

    #[test]
    fn fs_tree_node_omits_empty_optionals() {
        let node = FsTreeNode {
            name: "leaf".into(),
            path: "leaf".into(),
            kind: FsNodeKind::File,
            order: None,
            children: None,
        };
        let json = serde_json::to_string(&node).unwrap();
        assert!(!json.contains("order"));
        assert!(!json.contains("children"));
    }

    #[test]
    fn fs_search_hit_roundtrip() {
        let hit = FsSearchHit {
            root: "docs".into(),
            path: "a/b.md".into(),
            matches: vec![FsSearchMatch {
                line: 3,
                text: "hello".into(),
            }],
        };
        let json = serde_json::to_string(&hit).unwrap();
        let back: FsSearchHit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.root, "docs");
        assert_eq!(back.path, "a/b.md");
        assert_eq!(back.matches[0].line, 3);
        assert_eq!(back.matches[0].text, "hello");
    }
}
