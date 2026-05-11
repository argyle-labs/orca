use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::mcp::docs;
use crate::serve::api::llm as local_llm;

// ── list_roots ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListRootsArgs {}

pub struct ListRoots;

#[async_trait]
impl OrcaTool for ListRoots {
    const NAME: &'static str = "list_roots";
    const DESCRIPTION: &'static str =
        "List available documentation roots (rebuy, orca) with file counts and paths.";
    type Args = ListRootsArgs;
    async fn run(_args: ListRootsArgs, ctx: &ToolCtx) -> Result<String> {
        docs::list_roots(&ctx.config)
    }
}

// ── get_tree ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetTreeArgs {
    /// Root name: rebuy | orca
    pub root: String,
    /// Optional subpath within root (e.g. "admin-api" or "ai/claude/agents")
    pub path: Option<String>,
}

pub struct GetTree;

#[async_trait]
impl OrcaTool for GetTree {
    const NAME: &'static str = "get_tree";
    const DESCRIPTION: &'static str = "Get the compacted documentation tree for a root, optionally scoped to a subpath. \
         Returns a JSON tree of .md files.";
    type Args = GetTreeArgs;
    async fn run(args: GetTreeArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "root": args.root, "path": args.path });
        docs::get_tree(&v, &ctx.config)
    }
}

// ── read_doc ──────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ReadDocArgs {
    /// Root name: rebuy | orca | docs
    pub root: String,
    /// Path relative to root, without extension
    pub path: String,
    /// Pass "llm" to strip decorative markdown and reduce token usage
    pub format: Option<String>,
}

pub struct ReadDoc;

#[async_trait]
impl OrcaTool for ReadDoc {
    const NAME: &'static str = "read_doc";
    const DESCRIPTION: &'static str = "Read a documentation file by root and relative path \
         (e.g. root=rebuy, path=admin-api/README).";
    type Args = ReadDocArgs;
    async fn run(args: ReadDocArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "root": args.root, "path": args.path, "format": args.format });
        docs::read_doc(&v, &ctx.config)
    }
}

// ── search_docs ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct SearchDocsArgs {
    /// Search term (case-insensitive)
    pub query: String,
    /// Limit to root: rebuy | orca | docs | all (default: all)
    pub root: Option<String>,
    /// Pass "llm" to strip decorative markdown from matched lines
    pub format: Option<String>,
}

pub struct SearchDocs;

#[async_trait]
impl OrcaTool for SearchDocs {
    const NAME: &'static str = "search_docs";
    const DESCRIPTION: &'static str =
        "Search documentation files for a keyword across one or all roots.";
    type Args = SearchDocsArgs;
    async fn run(args: SearchDocsArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "query": args.query, "root": args.root, "format": args.format });
        let raw = docs::search_docs(&v, &ctx.config)?;
        if let Some(llm) = local_llm::discover_local_llm().await
            && let Some(enhanced) =
                local_llm::present_text_results(&llm, &args.query, &raw, 8000).await
        {
            return Ok(enhanced);
        }
        Ok(raw)
    }
}

// ── list_commands ─────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListCommandsArgs {}

pub struct ListCommands;

#[async_trait]
impl OrcaTool for ListCommands {
    const NAME: &'static str = "list_commands";
    const DESCRIPTION: &'static str =
        "List all Claude slash commands and skills from the orca vault.";
    type Args = ListCommandsArgs;
    async fn run(_args: ListCommandsArgs, ctx: &ToolCtx) -> Result<String> {
        docs::list_commands(&ctx.config)
    }
}

// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<ListRoots>()
        .register::<GetTree>()
        .register::<ReadDoc>()
        .register::<SearchDocs>()
        .register::<ListCommands>();
}
