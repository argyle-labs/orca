use anyhow::Result;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tool::{OrcaTool, ToolCtx};

use crate::mcp::handlers;
use crate::serve::api::llm as local_llm;

// ── list_agents ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListAgentsArgs {}

pub struct ListAgents;

#[async_trait]
impl OrcaTool for ListAgents {
    const NAME: &'static str = "list_agents";
    const DESCRIPTION: &'static str =
        "List all available orca agents with their names and descriptions.";
    type Args = ListAgentsArgs;
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

#[async_trait]
impl OrcaTool for GetAgent {
    const NAME: &'static str = "get_agent";
    const DESCRIPTION: &'static str =
        "Return the full system prompt for a named orca agent. Use this to invoke an agent \
         programmatically via Agent(general-purpose, prompt=<result>+task).";
    type Args = GetAgentArgs;
    async fn run(args: GetAgentArgs, ctx: &ToolCtx) -> Result<String> {
        let prompt = orca_agents::load_agent_prompt(&args.name, &ctx.config.agents_dir())
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

#[async_trait]
impl OrcaTool for GetConfig {
    const NAME: &'static str = "get_config";
    const DESCRIPTION: &'static str =
        "Read an orca configuration/reference document by name \
         (e.g. TOOL_RULES, DELEGATION, SEVERITY_RUBRIC, CANONICAL_SOURCES, CODING_RULES). \
         Call with no name to list available files.";
    type Args = GetConfigArgs;
    async fn run(args: GetConfigArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "name": args.name });
        handlers::get_config(&v, &ctx.config)
    }
}

// ── get_context ───────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetContextArgs {
    /// Project name (e.g. halvor, rebuy-db, dotfiles)
    pub project: String,
}

pub struct GetContext;

#[async_trait]
impl OrcaTool for GetContext {
    const NAME: &'static str = "get_context";
    const DESCRIPTION: &'static str =
        "Load the memory context for a orca project. Returns MEMORY.md index and all \
         memory files for the project.";
    type Args = GetContextArgs;
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

#[async_trait]
impl OrcaTool for SearchLogs {
    const NAME: &'static str = "search_logs";
    const DESCRIPTION: &'static str =
        "Search orca session history for a keyword. Returns matching log entries with \
         session ID, role, and content preview.";
    type Args = SearchLogsArgs;
    async fn run(args: SearchLogsArgs, ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        let v = json!({ "query": args.query });
        let raw = handlers::search_logs(&v, &ctx.config)?;
        if let Some(llm) = local_llm::discover_local_llm().await {
            if let Some(enhanced) =
                local_llm::present_text_results(&llm, &args.query, &raw, 8000).await
            {
                return Ok(enhanced);
            }
        }
        Ok(raw)
    }
}

// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut tool::ToolRegistry) {
    reg.register::<ListAgents>()
        .register::<GetAgent>()
        .register::<GetConfig>()
        .register::<GetContext>()
        .register::<SearchLogs>();
}
