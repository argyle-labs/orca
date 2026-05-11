use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::mcp::handlers;
use crate::serve::api::llm as local_llm;

// ── list_agents ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListAgentsArgs {}

pub struct ListAgents;

impl OrcaToolDef for ListAgents {
    const NAME: &'static str = "list_agents";
    const DESCRIPTION: &'static str =
        "List all available orca agents with their names and descriptions.";
    type Args = ListAgentsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for ListAgents {
    async fn run(_args: ListAgentsArgs, _ctx: &ToolCtx) -> Result<String> {
        handlers::agents()
    }
}
// ── get_agent ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetAgentArgs {
    /// Agent name (e.g. owl, fox, crow, bear)
    pub name: String,
}

pub struct GetAgent;

impl OrcaToolDef for GetAgent {
    const NAME: &'static str = "get_agent";
    const DESCRIPTION: &'static str = "Return the full system prompt for a named orca agent. Use this to invoke an agent \
         programmatically via Agent(general-purpose, prompt=<result>+task).";
    type Args = GetAgentArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetAgent {
    async fn run(args: GetAgentArgs, ctx: &ToolCtx) -> Result<String> {
        let prompt = crate::mcp::agent_resolve::load_agent_prompt(&args.name, &ctx.config)
            .ok_or_else(|| anyhow::anyhow!("agent not found: {}", args.name))?;
        Ok(prompt)
    }
}
// ── get_config ────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetConfigArgs {
    /// Config file name without extension (e.g. TOOL_RULES). Omit to list all available files.
    pub name: Option<String>,
}

pub struct GetConfig;

impl OrcaToolDef for GetConfig {
    const NAME: &'static str = "get_config";
    const DESCRIPTION: &'static str = "Read an orca configuration/reference document by name \
         (e.g. TOOL_RULES, DELEGATION, SEVERITY_RUBRIC, CANONICAL_SOURCES, CODING_RULES). \
         Call with no name to list available files.";
    type Args = GetConfigArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetConfig {
    async fn run(args: GetConfigArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "name": args.name });
        handlers::get_config(&v, &ctx.config)
    }
}
// ── get_context ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetContextArgs {
    /// Project name (e.g. meerkat, rebuy-db, dotfiles)
    pub project: String,
}

pub struct GetContext;

impl OrcaToolDef for GetContext {
    const NAME: &'static str = "get_context";
    const DESCRIPTION: &'static str = "Load the memory context for a orca project. Returns MEMORY.md index and all \
         memory files for the project.";
    type Args = GetContextArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetContext {
    async fn run(args: GetContextArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "project": args.project });
        handlers::get_context(&v, &ctx.config)
    }
}
// ── search_logs ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct SearchLogsArgs {
    /// Keyword to search for across all session logs
    pub query: String,
}

pub struct SearchLogs;

impl OrcaToolDef for SearchLogs {
    const NAME: &'static str = "search_logs";
    const DESCRIPTION: &'static str = "Search orca session history for a keyword. Returns matching log entries with \
         session ID, role, and content preview.";
    type Args = SearchLogsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for SearchLogs {
    async fn run(args: SearchLogsArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "query": args.query });
        let raw = handlers::search_logs(&v, &ctx.config)?;
        if let Some(llm) = local_llm::discover_local_llm().await
            && let Some(enhanced) =
                local_llm::present_text_results(&llm, &args.query, &raw, 8000).await
        {
            return Ok(enhanced);
        }
        Ok(raw)
    }
}
// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<ListAgents>()
        .register::<GetAgent>()
        .register::<GetConfig>()
        .register::<GetContext>()
        .register::<SearchLogs>();
}
