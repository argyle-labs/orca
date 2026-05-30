//! Agent tools — list, get prompt, config docs, project memory. Tool bodies
//! call directly into `crate::embedded` / `orca_utils` / `platform::profile_manager`
//! — no service-trait indirection.
//!
//! Note: session-log search lives in the `conversation` crate (it queries
//! conversation-owned data) — see `conversation::log_search`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

// ── Typed entities ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct MemoryFile {
    pub name: String,
    pub content: String,
}

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsOutput {
    pub agents: Vec<AgentEntry>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetAgentArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetAgentOutput {
    pub name: String,
    pub prompt: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetConfigArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetConfigOutput {
    pub available: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetContextArgs {
    pub project: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetContextOutput {
    pub project: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    pub files: Vec<MemoryFile>,
}

// ── Native tool bodies ──────────────────────────────────────────────────────

/// List all available orca agents with their names and descriptions.
#[orca_tool(domain = "system.agent", verb = "list")]
async fn list_agents(
    _args: ListAgentsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListAgentsOutput> {
    let agents = crate::embedded::list_embedded_agents()
        .into_iter()
        .map(|(name, description)| AgentEntry { name, description })
        .collect();
    Ok(ListAgentsOutput { agents })
}

/// Return the full system prompt for a named orca agent.
#[orca_tool(domain = "system.agent", verb = "get")]
async fn get_agent(args: GetAgentArgs, ctx: &contract::ToolCtx) -> anyhow::Result<GetAgentOutput> {
    let prompt = crate::resolve::load_agent_prompt(&args.name, &ctx.config)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {}", args.name))?;
    Ok(GetAgentOutput {
        name: args.name,
        prompt,
    })
}

/// Read an orca configuration/reference document by name.
#[orca_tool(domain = "system.agent", verb = "get-config")]
async fn get_config(
    args: GetConfigArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GetConfigOutput> {
    let available = contract::config::docs::list_basenames();
    let content = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .and_then(contract::config::docs::get);
    Ok(GetConfigOutput {
        available,
        name: args.name,
        content,
    })
}

/// Load the memory context for an orca project.
#[orca_tool(domain = "system.agent", verb = "get-context")]
async fn get_context(
    args: GetContextArgs,
    ctx: &contract::ToolCtx,
) -> anyhow::Result<GetContextOutput> {
    let dir = ctx.config.memory_root.join(&args.project);
    if !dir.exists() {
        return Ok(GetContextOutput {
            project: args.project,
            exists: false,
            index: None,
            files: Vec::new(),
        });
    }
    let index_path = dir.join("MEMORY.md");
    let index = if index_path.exists() {
        Some(std::fs::read_to_string(&index_path)?)
    } else {
        None
    };
    let mut entries: Vec<_> = std::fs::read_dir(&dir)?
        .flatten()
        .filter(|e| {
            let p = e.path();
            p.extension().map(|x| x == "md").unwrap_or(false)
                && p.file_name().map(|n| n != "MEMORY.md").unwrap_or(true)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let mut files = Vec::new();
    for f in entries {
        let path = f.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let content = std::fs::read_to_string(&path)?;
        files.push(MemoryFile { name, content });
    }
    Ok(GetContextOutput {
        project: args.project,
        exists: true,
        index,
        files,
    })
}
