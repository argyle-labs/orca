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

use orca_macro::orca_tool;

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
    pub exists: bool,
    pub file_count: u32,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsListArgs {
    /// Named root alias (e.g. "rebuy", "orca", "docs"). Omit to address path absolutely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Path within root, or absolute / `~/`-prefixed when no root.
    #[serde(default)]
    pub path: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsListOutput {
    pub entries: Vec<FsEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsSearchArgs {
    /// Case-insensitive search term.
    pub query: String,
    /// Limit to one root (e.g. "rebuy"|"orca"|"docs"). Default: search every registered root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// `"llm"` strips decorative markdown from matched lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchOutput {
    pub query: String,
    pub hits: Vec<FsSearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhanced_summary: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
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

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsRootsListArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct FsRootsListOutput {
    pub roots: Vec<FsRootEntry>,
}

mod native;

// ═══════════════════════════════════════════════════════════════════════════
// Tools — call free fns in `native` directly. No service trait.
// ═══════════════════════════════════════════════════════════════════════════

/// List the registered filesystem roots — named path aliases consumed by every other `fs.*` tool.
#[orca_tool(domain = "fs.roots", verb = "list", role = "read")]
async fn fs_roots_list(
    _args: FsRootsListArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<FsRootsListOutput> {
    Ok(FsRootsListOutput {
        roots: native::roots_list(&ctx.config).await?,
    })
}

/// One-level directory listing. Provide `root` for a named alias or omit it for an absolute / `~/`-prefixed path.
#[orca_tool(domain = "fs", verb = "list", role = "read")]
async fn fs_list(args: FsListArgs, ctx: &orca_contract::ToolCtx) -> anyhow::Result<FsListOutput> {
    Ok(FsListOutput {
        entries: native::list(&ctx.config, args.root.as_deref(), &args.path).await?,
    })
}

/// Recursive directory tree. Compacted by default; pass `raw=true` for the unmodified filesystem layout.
#[orca_tool(domain = "fs", verb = "tree", role = "read")]
async fn fs_tree(args: FsTreeArgs, ctx: &orca_contract::ToolCtx) -> anyhow::Result<FsTreeOutput> {
    Ok(FsTreeOutput {
        nodes: native::tree(
            &ctx.config,
            args.root.as_deref(),
            &args.path,
            args.raw.unwrap_or(false),
        )
        .await?,
    })
}

/// Read a text file. `format="llm"` strips decorative markdown; binary/multi-format reads are deferred to v2.
#[orca_tool(domain = "fs", verb = "read", role = "read")]
async fn fs_read(args: FsReadArgs, ctx: &orca_contract::ToolCtx) -> anyhow::Result<FsReadOutput> {
    let llm = args.format.as_deref() == Some("llm");
    let content = native::read(&ctx.config, args.root.as_deref(), &args.path, llm).await?;
    Ok(FsReadOutput {
        root: args.root,
        path: args.path,
        content,
    })
}

/// Case-insensitive line search across one or all registered roots. Optionally summarised by a local LLM.
#[orca_tool(domain = "fs", verb = "search", role = "read")]
async fn fs_search(
    args: FsSearchArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<FsSearchOutput> {
    let llm = args.format.as_deref() == Some("llm");
    let filter = args.root.as_deref().unwrap_or("all");
    let (hits, summary) = native::search(&ctx.config, &args.query, filter, llm).await?;
    Ok(FsSearchOutput {
        query: args.query,
        hits,
        enhanced_summary: summary,
    })
}

/// Metadata for a single path — kind (file/dir), byte size, existence flag.
#[orca_tool(domain = "fs", verb = "stat", role = "read")]
async fn fs_stat(args: FsStatArgs, ctx: &orca_contract::ToolCtx) -> anyhow::Result<FsStatOutput> {
    native::stat(&ctx.config, args.root.as_deref(), &args.path).await
}
