//! Generic filesystem primitives — `fs.{list,read,tree,search,stat}` plus
//! `fs.roots.list`. Replaces the docs-specific `namespace.doc.{tree,read,
//! search,full-tree,list-roots}` tools (slice 2 of crate-topology-v2).
//!
//! Roots are named path aliases registered in orca.db (see
//! [[project_fs_crate]]). When `root` is absent, `path` is absolute or
//! `~/`-prefixed.
//!
//! v1 handles text/markdown only; multi-format read (PDF/DOCX/XLSX/...)
//! deferred to v2 — see [[project_fs_crate]].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Typed entities ──────────────────────────────────────────────────────────

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FsNodeKind {
    File,
    Dir,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FsNodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsTreeNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FsNodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FsTreeNode>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsSearchMatch {
    pub line: u32,
    pub text: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsSearchHit {
    pub root: String,
    pub path: String,
    pub matches: Vec<FsSearchMatch>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsRootEntry {
    pub name: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
    pub exists: bool,
    pub file_count: u32,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FsListArgs {
    /// Named root alias (e.g. "orca", "docs"). Omit to address path absolutely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Path within root, or absolute / `~/`-prefixed when no root.
    #[serde(default)]
    pub path: String,
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FsListOutput {
    /// Populated when listing a directory (path supplied).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<FsEntry>,
    /// Populated when listing registered roots (no path/root supplied).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<FsRootEntry>,
    /// Populated alongside `roots` — global ignore patterns.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignore_patterns: Vec<String>,
    /// Opaque cursor for the next page of `entries`, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total entries across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FsTreeArgs {
    /// Named root alias. When omitted, `path` must be absolute or `~/`-prefixed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Subpath within root, or absolute path. Empty means the root itself.
    #[serde(default)]
    pub path: String,
    /// Pass `true` to skip compaction and return the raw layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsTreeOutput {
    pub nodes: Vec<FsTreeNode>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FsReadArgs {
    /// Named root alias. Omit to read by absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Path within root, or absolute path.
    pub path: String,
    /// `"llm"` strips decorative markdown to reduce tokens. `"raw"` returns
    /// bytes as base64 (binary support is v2). Default: plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsReadOutput {
    pub root: Option<String>,
    pub path: String,
    pub content: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FsSearchArgs {
    /// Case-insensitive search term.
    pub query: String,
    /// Limit to one root (e.g. "orca"|"docs"). Default: search every registered root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchOutput {
    pub query: String,
    pub hits: Vec<FsSearchHit>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FsStatArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsStatOutput {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FsNodeKind,
    pub size: u64,
    pub exists: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FsUpdateArgs {
    /// Write a file: provide `path` (absolute or `~/`-prefixed) + `content`.
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long)]
    pub content: Option<String>,

    /// Register/update a root: provide `register_root_name` + `register_root_path`
    /// (+ optional `register_root_description`).
    #[arg(long)]
    pub register_root_name: Option<String>,
    #[arg(long)]
    pub register_root_path: Option<String>,
    #[arg(long)]
    pub register_root_description: Option<String>,

    /// Add a global ignore pattern.
    #[arg(long)]
    pub add_ignore_pattern: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FsUpdateOutput {
    pub applied: Vec<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FsDeleteArgs {
    /// Delete a file at `path` (absolute or `~/`-prefixed).
    #[arg(long)]
    pub path: Option<String>,

    /// Unregister a root by name.
    #[arg(long)]
    pub unregister_root: Option<String>,

    /// Remove a global ignore pattern.
    #[arg(long)]
    pub remove_ignore_pattern: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct FsDeleteOutput {
    pub applied: Vec<String>,
}

// ═══════════════════════════════════════════════════════════════════════════
// Tools — call free fns in the crate root directly. No service trait.
// ═══════════════════════════════════════════════════════════════════════════

/// List filesystem resources. No args → registered roots + global ignore
/// patterns. With `path` (and optional `root`) → directory contents at that path.
#[orca_tool(domain = "files", verb = "list", role = "read")]
async fn fs_list(args: FsListArgs, ctx: &contract::ToolCtx) -> anyhow::Result<FsListOutput> {
    let mut out = FsListOutput::default();
    if args.root.is_none() && args.path.is_empty() {
        out.roots = crate::roots_list(&ctx.config).await?;
        let conn = db::open_default()?;
        out.ignore_patterns = crate::docs::list_ignore_patterns(&conn)?;
    } else {
        let mut entries = crate::list(&ctx.config, args.root.as_deref(), &args.path).await?;
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let params = contract::paging::PageParams {
            limit: args.limit,
            cursor: args.cursor,
        };
        let page = contract::paging::Page::from_slice(entries, &params);
        out.entries = page.items;
        out.next_cursor = page.next_cursor;
        out.total = page.total;
    }
    Ok(out)
}

/// Recursive directory tree. Compacted by default; pass `raw=true` for the unmodified filesystem layout.
#[orca_tool(domain = "files", verb = "tree", role = "read")]
async fn fs_tree(args: FsTreeArgs, ctx: &contract::ToolCtx) -> anyhow::Result<FsTreeOutput> {
    Ok(FsTreeOutput {
        nodes: crate::tree(
            &ctx.config,
            args.root.as_deref(),
            &args.path,
            args.raw.unwrap_or(false),
        )
        .await?,
    })
}

/// Read a text file. `format="llm"` strips decorative markdown; binary/multi-format reads are deferred to v2.
#[orca_tool(domain = "files", verb = "read", role = "read")]
async fn fs_read(args: FsReadArgs, ctx: &contract::ToolCtx) -> anyhow::Result<FsReadOutput> {
    let llm = args.format.as_deref() == Some("llm");
    let content = crate::read(&ctx.config, args.root.as_deref(), &args.path, llm).await?;
    Ok(FsReadOutput {
        root: args.root,
        path: args.path,
        content,
    })
}

/// Case-insensitive line search across one or all registered roots. Returns hits only —
/// LLM summarisation surface dropped 2026-05-29; callers can format hits themselves.
#[orca_tool(domain = "files", verb = "search", role = "read")]
async fn fs_search(args: FsSearchArgs, ctx: &contract::ToolCtx) -> anyhow::Result<FsSearchOutput> {
    let filter = args.root.as_deref().unwrap_or("all");
    let hits = crate::search(&ctx.config, &args.query, filter).await?;
    Ok(FsSearchOutput {
        query: args.query,
        hits,
    })
}

/// Metadata for a single path — kind (file/dir), byte size, existence flag.
#[orca_tool(domain = "files", verb = "stat", role = "read")]
async fn fs_stat(args: FsStatArgs, ctx: &contract::ToolCtx) -> anyhow::Result<FsStatOutput> {
    crate::stat(&ctx.config, args.root.as_deref(), &args.path).await
}

/// [MUTATES STATE] Combine any of: write a file (`path` + `content`),
/// register/update a root (`register_root_*`), add a global ignore pattern
/// (`add_ignore_pattern`).
#[orca_tool(domain = "files", verb = "update")]
async fn fs_update(args: FsUpdateArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<FsUpdateOutput> {
    let mut out = FsUpdateOutput::default();

    match (args.path.as_deref(), args.content.as_deref()) {
        (Some(p), Some(c)) => {
            let written = crate::ops::write_file(p, c)?;
            out.applied.push(format!("wrote:{written}"));
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!("file write needs both `path` and `content`");
        }
        (None, None) => {}
    }

    if let Some(name) = &args.register_root_name {
        let path = args
            .register_root_path
            .clone()
            .ok_or_else(|| anyhow::anyhow!("register_root_path required"))?;
        let row = crate::docs::RootRow {
            name: name.clone(),
            path,
            description: args.register_root_description.clone(),
            enabled: true,
        };
        let conn = db::open_default()?;
        crate::docs::upsert_root(&conn, &row)?;
        out.applied.push(format!("root-upserted:{name}"));
    }

    if let Some(pattern) = &args.add_ignore_pattern {
        let conn = db::open_default()?;
        let changed = crate::docs::add_ignore_pattern(&conn, pattern)?;
        out.applied.push(format!(
            "pattern-added:{pattern}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }

    if out.applied.is_empty() {
        anyhow::bail!("no files.update operation specified");
    }
    Ok(out)
}

/// [MUTATES STATE] Combine any of: delete a file (`path`), unregister a root
/// (`unregister_root`), remove a global ignore pattern (`remove_ignore_pattern`).
#[orca_tool(domain = "files", verb = "delete")]
async fn fs_delete(args: FsDeleteArgs, _ctx: &contract::ToolCtx) -> anyhow::Result<FsDeleteOutput> {
    let mut out = FsDeleteOutput::default();

    if let Some(p) = &args.path {
        let resolved = crate::ops::expand_tilde(p);
        crate::ops::remove(std::path::Path::new(&resolved))?;
        out.applied.push(format!("file-deleted:{resolved}"));
    }

    if let Some(name) = &args.unregister_root {
        let conn = db::open_default()?;
        let changed = crate::docs::remove_root(&conn, name)?;
        out.applied.push(format!(
            "root-removed:{name}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }

    if let Some(pattern) = &args.remove_ignore_pattern {
        let conn = db::open_default()?;
        let changed = crate::docs::remove_ignore_pattern(&conn, pattern)?;
        out.applied.push(format!(
            "pattern-removed:{pattern}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }

    if out.applied.is_empty() {
        anyhow::bail!("no files.delete operation specified");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use contract::config::{Config, Model};
    use std::path::PathBuf;
    use std::sync::Arc;

    // ── ctx / scratch helpers (mirrors lib.rs test style) ────────────────────

    fn ctx() -> contract::ToolCtx {
        contract::ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/orca-files-tools-test.db"),
            ports: Default::default(),
        }))
    }

    fn scratch(tag: &str) -> PathBuf {
        let mut base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("orca-files-tools-{tag}-{nanos}-{:p}", &base));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    // ── fs_list ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_list_directory_paginates_and_sorts() {
        let dir = scratch("list");
        for n in ["b.txt", "a.txt", "c.txt"] {
            std::fs::write(dir.join(n), "x").unwrap();
        }
        let args = FsListArgs {
            root: None,
            path: dir.to_string_lossy().into_owned(),
            limit: Some(2),
            cursor: None,
        };
        let out = fs_list(args, &ctx()).await.unwrap();
        // Roots are only populated with no path; directory listing skips them.
        assert!(out.roots.is_empty());
        // Limit clamps the page to 2 sorted-by-path entries; total spans all 3.
        assert_eq!(out.entries.len(), 2);
        assert_eq!(out.entries[0].name, "a.txt");
        assert_eq!(out.entries[1].name, "b.txt");
        assert_eq!(out.total, Some(3));
        assert!(out.next_cursor.is_some());
    }

    #[tokio::test]
    async fn fs_list_rejects_relative_path() {
        let args = FsListArgs {
            root: None,
            path: "not/absolute".into(),
            limit: None,
            cursor: None,
        };
        let err = match fs_list(args, &ctx()).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("must be absolute"), "got: {err}");
    }

    // ── fs_tree ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_tree_returns_nodes_for_directory() {
        let dir = scratch("tree");
        std::fs::write(dir.join("only.md"), "# hi").unwrap();
        let args = FsTreeArgs {
            root: None,
            path: dir.to_string_lossy().into_owned(),
            raw: Some(true),
        };
        let out = fs_tree(args, &ctx()).await.unwrap();
        // Path is the root-relative filename; the display name derives from the
        // markdown title ("# hi" → "hi"), so match on path.
        assert!(
            out.nodes.iter().any(|n| n.path == "only.md"),
            "expected only.md in {:?}",
            out.nodes.iter().map(|n| &n.path).collect::<Vec<_>>()
        );
    }

    // ── fs_read ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_read_plain_returns_verbatim_content() {
        let dir = scratch("read");
        let file = dir.join("doc.md");
        std::fs::write(&file, "# Title\n\nbody").unwrap();
        let args = FsReadArgs {
            root: None,
            path: file.to_string_lossy().into_owned(),
            format: None,
        };
        let out = fs_read(args, &ctx()).await.unwrap();
        assert_eq!(out.content, "# Title\n\nbody");
        assert!(out.root.is_none());
        assert_eq!(out.path, file.to_string_lossy());
    }

    #[tokio::test]
    async fn fs_read_missing_file_errors() {
        let args = FsReadArgs {
            root: None,
            path: "/tmp/orca-files-tools-nope-xyz-999.md".into(),
            format: Some("llm".into()),
        };
        assert!(fs_read(args, &ctx()).await.is_err());
    }

    // ── fs_search ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_search_echoes_query_and_empty_on_unknown_root() {
        // A filter matching no registered root and not "docs" yields no hits.
        let args = FsSearchArgs {
            query: "zzz_unlikely_term_qwxyz".into(),
            root: Some("no_such_root_filter".into()),
        };
        let out = fs_search(args, &ctx()).await.unwrap();
        assert_eq!(out.query, "zzz_unlikely_term_qwxyz");
        assert!(out.hits.is_empty());
    }

    // ── fs_stat ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_stat_reports_file_size_and_existence() {
        let dir = scratch("stat");
        let file = dir.join("sized.txt");
        std::fs::write(&file, "12345").unwrap();
        let args = FsStatArgs {
            root: None,
            path: file.to_string_lossy().into_owned(),
        };
        let out = fs_stat(args, &ctx()).await.unwrap();
        assert!(out.exists);
        assert_eq!(out.size, 5);
        assert!(matches!(out.kind, FsNodeKind::File));
        assert_eq!(out.name, "sized.txt");
    }

    #[tokio::test]
    async fn fs_stat_missing_marks_not_exists() {
        let dir = scratch("stat-missing");
        let file = dir.join("ghost.txt");
        let args = FsStatArgs {
            root: None,
            path: file.to_string_lossy().into_owned(),
        };
        let out = fs_stat(args, &ctx()).await.unwrap();
        assert!(!out.exists);
        assert_eq!(out.size, 0);
    }

    // ── fs_update ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_update_writes_file() {
        let dir = scratch("update");
        let file = dir.join("out.txt");
        let args = FsUpdateArgs {
            path: Some(file.to_string_lossy().into_owned()),
            content: Some("hello".into()),
            ..Default::default()
        };
        let out = fs_update(args, &ctx()).await.unwrap();
        assert_eq!(out.applied.len(), 1);
        assert!(out.applied[0].starts_with("wrote:"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
    }

    #[tokio::test]
    async fn fs_update_partial_file_args_error() {
        let args = FsUpdateArgs {
            path: Some("/tmp/whatever.txt".into()),
            content: None,
            ..Default::default()
        };
        let err = match fs_update(args, &ctx()).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("needs both"), "got: {err}");
    }

    #[tokio::test]
    async fn fs_update_no_op_errors() {
        let err = match fs_update(FsUpdateArgs::default(), &ctx()).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("no files.update operation"),
            "got: {err}"
        );
    }

    // ── fs_delete ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fs_delete_removes_file() {
        let dir = scratch("delete");
        let file = dir.join("gone.txt");
        std::fs::write(&file, "bye").unwrap();
        let args = FsDeleteArgs {
            path: Some(file.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let out = fs_delete(args, &ctx()).await.unwrap();
        assert_eq!(out.applied.len(), 1);
        assert!(out.applied[0].starts_with("file-deleted:"));
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn fs_delete_no_op_errors() {
        let err = match fs_delete(FsDeleteArgs::default(), &ctx()).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("no files.delete operation"),
            "got: {err}"
        );
    }
}
