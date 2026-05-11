//! Docs domain tools — root listing, file tree, read, search, commands.
//! Run impls dispatch through `services::docs::DocsService`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Typed entities ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DocRootEntry {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub doc_count: u32,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DocNodeKind {
    File,
    Dir,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocSearchMatch {
    pub line: u32,
    pub text: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct DocSearchHit {
    pub root: String,
    pub path: String,
    pub matches: Vec<DocSearchMatch>,
}

// ── list_roots ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRootsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListRootsOutput {
    pub roots: Vec<DocRootEntry>,
}

pub struct ListRoots;
impl OrcaToolDef for ListRoots {
    const NAME: &'static str = "list_roots";
    const DESCRIPTION: &'static str =
        "List available documentation roots (rebuy, orca) with file counts and paths.";
    type Args = ListRootsArgs;
    type Output = ListRootsOutput;
}

// ── get_tree ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetTreeArgs {
    /// Root name: rebuy | orca | docs
    pub root: String,
    /// Optional subpath within root (e.g. "admin-api" or "ai/claude/agents")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetTreeOutput {
    pub root: String,
    pub path: Option<String>,
    pub nodes: Vec<DocTreeNode>,
}

pub struct GetTree;
impl OrcaToolDef for GetTree {
    const NAME: &'static str = "get_tree";
    const DESCRIPTION: &'static str = "Get the compacted documentation tree for a root, optionally \
         scoped to a subpath. Returns a typed tree of .md files.";
    type Args = GetTreeArgs;
    type Output = GetTreeOutput;
}

// ── read_doc ────────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ReadDocOutput {
    pub root: String,
    pub path: String,
    pub content: String,
}

pub struct ReadDoc;
impl OrcaToolDef for ReadDoc {
    const NAME: &'static str = "read_doc";
    const DESCRIPTION: &'static str = "Read a documentation file by root and relative path \
         (e.g. root=rebuy, path=admin-api/README).";
    type Args = ReadDocArgs;
    type Output = ReadDocOutput;
}

// ── search_docs ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchDocsOutput {
    pub query: String,
    pub hits: Vec<DocSearchHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhanced_summary: Option<String>,
}

pub struct SearchDocs;
impl OrcaToolDef for SearchDocs {
    const NAME: &'static str = "search_docs";
    const DESCRIPTION: &'static str =
        "Search documentation files for a keyword across one or all roots.";
    type Args = SearchDocsArgs;
    type Output = SearchDocsOutput;
}

// ── list_commands ───────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListCommandsOutput {
    pub commands: Vec<String>,
}

pub struct ListCommands;
impl OrcaToolDef for ListCommands {
    const NAME: &'static str = "list_commands";
    const DESCRIPTION: &'static str =
        "List all Claude slash commands and skills from the orca vault.";
    type Args = ListCommandsArgs;
    type Output = ListCommandsOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::docs as svc_docs;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn svc_docs::DocsService>> {
        ctx.service::<Arc<dyn svc_docs::DocsService>>()
    }

    fn data_to_node(d: svc_docs::DocTreeNodeData) -> DocTreeNode {
        DocTreeNode {
            name: d.name,
            path: d.path,
            kind: match d.kind {
                svc_docs::DocNodeKind::File => DocNodeKind::File,
                svc_docs::DocNodeKind::Dir => DocNodeKind::Dir,
            },
            order: d.order,
            children: d
                .children
                .map(|cs| cs.into_iter().map(data_to_node).collect()),
        }
    }

    #[async_trait]
    impl OrcaTool for ListRoots {
        async fn run(_args: ListRootsArgs, ctx: &ToolCtx) -> Result<ListRootsOutput> {
            let roots = svc(ctx)?
                .list_roots()
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
    }

    #[async_trait]
    impl OrcaTool for GetTree {
        async fn run(args: GetTreeArgs, ctx: &ToolCtx) -> Result<GetTreeOutput> {
            let data = svc(ctx)?.get_tree(&args.root, args.path.as_deref()).await?;
            Ok(GetTreeOutput {
                root: args.root,
                path: args.path,
                nodes: data.into_iter().map(data_to_node).collect(),
            })
        }
    }

    #[async_trait]
    impl OrcaTool for ReadDoc {
        async fn run(args: ReadDocArgs, ctx: &ToolCtx) -> Result<ReadDocOutput> {
            let llm = args.format.as_deref() == Some("llm");
            let content = svc(ctx)?.read_doc(&args.root, &args.path, llm).await?;
            Ok(ReadDocOutput {
                root: args.root,
                path: args.path,
                content,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for SearchDocs {
        async fn run(args: SearchDocsArgs, ctx: &ToolCtx) -> Result<SearchDocsOutput> {
            let filter = args.root.as_deref().unwrap_or("all");
            let llm = args.format.as_deref() == Some("llm");
            let data = svc(ctx)?.search_docs(&args.query, filter, llm).await?;
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
    }

    #[async_trait]
    impl OrcaTool for ListCommands {
        async fn run(_args: ListCommandsArgs, ctx: &ToolCtx) -> Result<ListCommandsOutput> {
            let commands = svc(ctx)?.list_commands().await?;
            Ok(ListCommandsOutput { commands })
        }
    }
}
