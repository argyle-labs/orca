//! Docs domain tools — root listing, file tree, read, search, commands.
//! Tools call free functions in `crate::native_support_docs` directly per
//! [[feedback_no_indirection]]. Will fold into a generic `fs` crate later —
//! see [[project_fs_crate]].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;

// ── Typed entities ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DocRootEntry {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub doc_count: u32,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DocNodeKind {
    File,
    Dir,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocTreeNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: DocNodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DocTreeNode>>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocSearchMatch {
    pub line: u32,
    pub text: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocSearchHit {
    pub root: String,
    pub path: String,
    pub matches: Vec<DocSearchMatch>,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRootsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRootsOutput {
    pub roots: Vec<DocRootEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetTreeArgs {
    /// Root name: rebuy | orca | docs
    pub root: String,
    /// Optional subpath within root (e.g. "admin-api" or "ai/claude/agents")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetTreeOutput {
    pub root: String,
    pub path: Option<String>,
    pub nodes: Vec<DocTreeNode>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetFullTreeArgs {
    /// Pass `true` to skip compaction and return the raw filesystem tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<bool>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocRootTreeEntry {
    pub root: String,
    pub nodes: Vec<DocTreeNode>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetFullTreeOutput {
    pub roots: Vec<DocRootTreeEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadDocArgs {
    /// Root name: rebuy | orca | docs
    pub root: String,
    /// Path relative to root, without extension
    pub path: String,
    /// Pass "llm" to strip decorative markdown and reduce token usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadDocOutput {
    pub root: String,
    pub path: String,
    pub content: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchDocsArgs {
    /// Case-insensitive search term.
    pub query: String,
    /// Limit to root: rebuy | orca | docs | all (default: all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Pass "llm" to strip decorative markdown from matched lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchDocsOutput {
    pub query: String,
    pub hits: Vec<DocSearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhanced_summary: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsOutput {
    pub commands: Vec<String>,
}

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn data_to_node(d: crate::docs::DocTreeNodeData) -> DocTreeNode {
    DocTreeNode {
        name: d.name,
        path: d.path,
        kind: match d.kind {
            crate::docs::DocNodeKind::File => DocNodeKind::File,
            crate::docs::DocNodeKind::Dir => DocNodeKind::Dir,
        },
        order: d.order,
        children: d
            .children
            .map(|cs| cs.into_iter().map(data_to_node).collect()),
    }
}

/// List available documentation roots (rebuy, orca) with file counts and paths.
#[orca_tool(domain = "namespace.doc", verb = "list-roots")]
async fn list_roots(
    _args: ListRootsArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListRootsOutput> {
    let roots = crate::native_support_docs::list_roots(&ctx.config)
        .await?
        .into_iter()
        .map(|r| DocRootEntry {
            name: r.name,
            path: r.path,
            exists: r.exists,
            doc_count: r.doc_count as u32,
        })
        .collect();
    Ok(ListRootsOutput { roots })
}

/// Get the compacted documentation tree for a root, optionally scoped to a
/// subpath. Returns a typed tree of .md files.
#[orca_tool(domain = "namespace.doc", verb = "tree")]
async fn get_tree(
    args: GetTreeArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetTreeOutput> {
    let data =
        crate::native_support_docs::get_tree(&ctx.config, &args.root, args.path.as_deref()).await?;
    Ok(GetTreeOutput {
        root: args.root,
        path: args.path,
        nodes: data.into_iter().map(data_to_node).collect(),
    })
}

/// Multi-root documentation tree — every registered root in one call. When
/// `raw` is true, returns the uncompacted filesystem layout.
#[orca_tool(domain = "namespace.doc", verb = "full-tree")]
async fn get_full_tree(
    args: GetFullTreeArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GetFullTreeOutput> {
    let raw = args.raw.unwrap_or(false);
    let data = crate::native_support_docs::get_full_tree(raw).await?;
    let roots = data
        .into_iter()
        .map(|r| DocRootTreeEntry {
            root: r.root,
            nodes: r.nodes.into_iter().map(data_to_node).collect(),
        })
        .collect();
    Ok(GetFullTreeOutput { roots })
}

/// Read a documentation file by root and relative path (e.g. root=rebuy,
/// path=admin-api/README).
#[orca_tool(domain = "namespace.doc", verb = "read")]
async fn read_doc(
    args: ReadDocArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ReadDocOutput> {
    let llm = args.format.as_deref() == Some("llm");
    let content =
        crate::native_support_docs::read_doc(&ctx.config, &args.root, &args.path, llm).await?;
    Ok(ReadDocOutput {
        root: args.root,
        path: args.path,
        content,
    })
}

/// Search documentation files for a keyword across one or all roots.
#[orca_tool(domain = "namespace.doc", verb = "search")]
async fn search_docs(
    args: SearchDocsArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SearchDocsOutput> {
    let filter = args.root.as_deref().unwrap_or("all");
    let llm = args.format.as_deref() == Some("llm");
    let data =
        crate::native_support_docs::search_docs(&ctx.config, &args.query, filter, llm).await?;
    let hits = data
        .hits
        .into_iter()
        .map(|h| DocSearchHit {
            root: h.root,
            path: h.path,
            matches: h
                .matches
                .into_iter()
                .map(|m| DocSearchMatch {
                    line: m.line,
                    text: m.text,
                })
                .collect(),
        })
        .collect();
    Ok(SearchDocsOutput {
        query: args.query,
        hits,
        enhanced_summary: data.enhanced_summary,
    })
}

/// List all Claude slash commands and skills from the orca vault.
#[orca_tool(domain = "namespace.doc", verb = "list-commands")]
async fn list_commands(
    _args: ListCommandsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListCommandsOutput> {
    let commands = crate::native_support_docs::list_commands().await?;
    Ok(ListCommandsOutput { commands })
}

// ─── Data shapes used by native dispatch ──────────────────────────────

#[derive(Clone)]
pub struct DocRootSummary {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub doc_count: usize,
}

#[derive(Clone)]
pub struct DocTreeNodeData {
    pub name: String,
    pub path: String,
    pub kind: DocNodeKind,
    pub order: Option<u32>,
    pub children: Option<Vec<DocTreeNodeData>>,
}

#[derive(Clone)]
pub struct DocRootTree {
    pub root: String,
    pub nodes: Vec<DocTreeNodeData>,
}

#[derive(Clone)]
pub struct SearchDocMatch {
    pub line: u32,
    pub text: String,
}

#[derive(Clone)]
pub struct SearchDocHit {
    pub root: String,
    pub path: String,
    pub matches: Vec<SearchDocMatch>,
}

#[derive(Clone)]
pub struct SearchDocsData {
    pub hits: Vec<SearchDocHit>,
    pub enhanced_summary: Option<String>,
}

