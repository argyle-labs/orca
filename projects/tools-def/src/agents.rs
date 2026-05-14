//! Agents domain tools — list, get prompt, get_config docs, project memory,
//! log search. Run impls dispatch through `services::agents::AgentsService`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

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

// ── Args / Outputs ──────────────────────────────────────────────────────────

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

// ── Native dispatch ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
fn agents_svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::agents::AgentsService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::agents::AgentsService>>()
}

/// List all available orca agents with their names and descriptions.
#[orca_tool(domain = "agents", verb = "list")]
async fn list_agents(
    _args: ListAgentsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListAgentsOutput> {
    let agents = agents_svc(ctx)?
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

/// Return the full system prompt for a named orca agent. Use this to invoke an
/// agent programmatically via Agent(general-purpose, prompt=<result>+task).
#[orca_tool(domain = "agents", verb = "get")]
async fn get_agent(
    args: GetAgentArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetAgentOutput> {
    let prompt = agents_svc(ctx)?
        .get_agent_prompt(&args.name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("agent not found: {}", args.name))?;
    Ok(GetAgentOutput {
        name: args.name,
        prompt,
    })
}

/// Read an orca configuration/reference document by name (e.g. TOOL_RULES,
/// DELEGATION, SEVERITY_RUBRIC, CANONICAL_SOURCES, CODING_RULES). Call with no
/// name to list available files.
#[orca_tool(domain = "agents", verb = "get-config")]
async fn get_config(
    args: GetConfigArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetConfigOutput> {
    let s = agents_svc(ctx)?;
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

/// Load the memory context for an orca project. Returns the MEMORY.md index
/// and all memory files for the project.
#[orca_tool(domain = "agents", verb = "get-context")]
async fn get_context(
    args: GetContextArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetContextOutput> {
    match agents_svc(ctx)?.read_project_memory(&args.project).await? {
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

/// Search orca session history for a keyword. Returns matching log entries
/// with session ID, role, and content preview.
#[orca_tool(domain = "agents", verb = "search-logs")]
async fn search_logs(
    args: SearchLogsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SearchLogsOutput> {
    let data = agents_svc(ctx)?.search_logs(&args.query, 20).await?;
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
