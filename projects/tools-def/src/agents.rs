//! Agents domain tools — list, get prompt, get_config docs, project memory,
//! log search. Run impls dispatch through `services::agents::AgentsService`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

// ── Typed entities ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MemoryFile {
    pub name: String,
    pub content: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogMatchEntry {
    pub session: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub content_preview: String,
    pub important: bool,
}

// ── list_agents ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsOutput {
    pub agents: Vec<AgentEntry>,
}

pub struct ListAgents;
impl OrcaToolDef for ListAgents {
    const NAME: &'static str = "list_agents";
    const DESCRIPTION: &'static str =
        "List all available orca agents with their names and descriptions.";
    type Args = ListAgentsArgs;
    type Output = ListAgentsOutput;
}

// ── get_agent ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetAgentArgs {
    /// Agent name (e.g. owl, fox, crow, bear)
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetAgentOutput {
    pub name: String,
    pub prompt: String,
}

pub struct GetAgent;
impl OrcaToolDef for GetAgent {
    const NAME: &'static str = "get_agent";
    const DESCRIPTION: &'static str = "Return the full system prompt for a named orca agent. \
         Use this to invoke an agent programmatically via Agent(general-purpose, \
         prompt=<result>+task).";
    type Args = GetAgentArgs;
    type Output = GetAgentOutput;
}

// ── get_config ──────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetConfigArgs {
    /// Config file basename without extension (e.g. TOOL_RULES). Omit to
    /// list all available basenames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetConfigOutput {
    /// All available config-doc basenames.
    pub available: Vec<String>,
    /// The basename that was requested (echoed back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Content when `name` was provided and the doc was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

pub struct GetConfig;
impl OrcaToolDef for GetConfig {
    const NAME: &'static str = "get_config";
    const DESCRIPTION: &'static str = "Read an orca configuration/reference document by name \
         (e.g. TOOL_RULES, DELEGATION, SEVERITY_RUBRIC, CANONICAL_SOURCES, CODING_RULES). \
         Call with no name to list available files.";
    type Args = GetConfigArgs;
    type Output = GetConfigOutput;
}

// ── get_context ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetContextArgs {
    /// Project name (e.g. meerkat, rebuy-db, dotfiles)
    pub project: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetContextOutput {
    pub project: String,
    /// `true` when the memory directory exists for the project.
    pub exists: bool,
    /// MEMORY.md index content (if present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// All non-index .md memory files for the project.
    pub files: Vec<MemoryFile>,
}

pub struct GetContext;
impl OrcaToolDef for GetContext {
    const NAME: &'static str = "get_context";
    const DESCRIPTION: &'static str = "Load the memory context for an orca project. Returns the \
         MEMORY.md index and all memory files for the project.";
    type Args = GetContextArgs;
    type Output = GetContextOutput;
}

// ── search_logs ─────────────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SearchLogsArgs {
    /// Keyword to search for across all session logs
    pub query: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchLogsOutput {
    pub query: String,
    pub matches: Vec<LogMatchEntry>,
    /// LLM-generated summary when a local model was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhanced_summary: Option<String>,
}

pub struct SearchLogs;
impl OrcaToolDef for SearchLogs {
    const NAME: &'static str = "search_logs";
    const DESCRIPTION: &'static str = "Search orca session history for a keyword. Returns matching \
         log entries with session ID, role, and content preview.";
    type Args = SearchLogsArgs;
    type Output = SearchLogsOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::agents::AgentsService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn AgentsService>> {
        ctx.service::<Arc<dyn AgentsService>>()
    }

    #[async_trait]
    impl OrcaTool for ListAgents {
        async fn run(_args: ListAgentsArgs, ctx: &ToolCtx) -> Result<ListAgentsOutput> {
            let agents = svc(ctx)?
                .list_agents()
                .await?
                .into_iter()
                .map(|a| AgentEntry {
                    name: a.name,
                    description: a.description,
                })
                .collect();
            Ok(ListAgentsOutput { agents })
        }
    }

    #[async_trait]
    impl OrcaTool for GetAgent {
        async fn run(args: GetAgentArgs, ctx: &ToolCtx) -> Result<GetAgentOutput> {
            let prompt = svc(ctx)?
                .get_agent_prompt(&args.name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("agent not found: {}", args.name))?;
            Ok(GetAgentOutput {
                name: args.name,
                prompt,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for GetConfig {
        async fn run(args: GetConfigArgs, ctx: &ToolCtx) -> Result<GetConfigOutput> {
            let s = svc(ctx)?;
            let available = s.list_config_docs().await?;
            let content = if let Some(n) = args
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
            {
                s.read_config_doc(n).await?
            } else {
                None
            };
            Ok(GetConfigOutput {
                available,
                name: args.name,
                content,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for GetContext {
        async fn run(args: GetContextArgs, ctx: &ToolCtx) -> Result<GetContextOutput> {
            match svc(ctx)?.read_project_memory(&args.project).await? {
                Some(mem) => Ok(GetContextOutput {
                    project: args.project,
                    exists: true,
                    index: mem.index,
                    files: mem
                        .files
                        .into_iter()
                        .map(|f| MemoryFile {
                            name: f.name,
                            content: f.content,
                        })
                        .collect(),
                }),
                None => Ok(GetContextOutput {
                    project: args.project,
                    exists: false,
                    index: None,
                    files: Vec::new(),
                }),
            }
        }
    }

    #[async_trait]
    impl OrcaTool for SearchLogs {
        async fn run(args: SearchLogsArgs, ctx: &ToolCtx) -> Result<SearchLogsOutput> {
            let data = svc(ctx)?.search_logs(&args.query, 20).await?;
            let matches = data
                .matches
                .into_iter()
                .map(|m| LogMatchEntry {
                    session: m.session,
                    role: m.role,
                    agent: m.agent,
                    content_preview: m.content_preview,
                    important: m.important,
                })
                .collect();
            Ok(SearchLogsOutput {
                query: args.query,
                matches,
                enhanced_summary: data.enhanced_summary,
            })
        }
    }
}
