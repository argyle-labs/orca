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
        out.ignore_patterns = db::docs::list_ignore_patterns(&conn)?;
    } else {
        out.entries = crate::list(&ctx.config, args.root.as_deref(), &args.path).await?;
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
        let row = db::docs::RootRow {
            name: name.clone(),
            path,
            description: args.register_root_description.clone(),
            enabled: true,
        };
        let conn = db::open_default()?;
        db::docs::upsert_root(&conn, &row)?;
        out.applied.push(format!("root-upserted:{name}"));
    }

    if let Some(pattern) = &args.add_ignore_pattern {
        let conn = db::open_default()?;
        let changed = db::docs::add_ignore_pattern(&conn, pattern)?;
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
        let changed = db::docs::remove_root(&conn, name)?;
        out.applied.push(format!(
            "root-removed:{name}:{}",
            if changed { "yes" } else { "absent" }
        ));
    }

    if let Some(pattern) = &args.remove_ignore_pattern {
        let conn = db::open_default()?;
        let changed = db::docs::remove_ignore_pattern(&conn, pattern)?;
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
