//! Agent tools — list agents, get agent prompt. Filesystem reads (config
//! docs, project memory) live in the `files` crate / `namespace` crate
//! respectively — they aren't agent concerns and shouldn't masquerade as
//! `agent.*` tools.
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

// ── Args / Outputs ──────────────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListAgentsArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListAgentsOutput {
    pub agents: Vec<AgentEntry>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct GetAgentArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetAgentOutput {
    pub name: String,
    pub prompt: String,
}

// ── Native tool bodies ──────────────────────────────────────────────────────

/// List all available orca agents with their names and descriptions.
#[orca_tool(domain = "agent", verb = "list")]
async fn list_agents(
    args: ListAgentsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListAgentsOutput> {
    // Compose across every registered provider (embedded baseline + external
    // repo sources + the plugin-supplied `argyle-labs/agents` roster) — not just
    // the embedded set, which core no longer populates (the roster is contributed
    // by the agents plugin at runtime). Reading only embedded agents made this
    // list always empty on a roster-less core.
    let mut agents: Vec<AgentEntry> = crate::compose_agents()
        .into_iter()
        .map(|a| AgentEntry {
            description: crate::embedded::frontmatter_field_from_str(&a.body, "description")
                .unwrap_or_default(),
            name: a.name,
        })
        .collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(agents, &params);
    Ok(ListAgentsOutput {
        agents: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
    })
}

/// Return the full system prompt for a named orca agent.
#[orca_tool(domain = "agent", verb = "get")]
async fn get_agent(args: GetAgentArgs, ctx: &contract::ToolCtx) -> anyhow::Result<GetAgentOutput> {
    // Prefer the composed roster (embedded + external repos + plugin-supplied),
    // matching `agent.list`. Fall back to the profile-aware on-disk search for
    // agents that only exist as files. `body` carries frontmatter; strip it to
    // preserve the prior "prompt only" semantics of the filesystem path.
    let prompt = crate::compose_agents()
        .into_iter()
        .find(|a| a.name == args.name)
        .map(|a| crate::embedded::strip_frontmatter(&a.body))
        .or_else(|| crate::resolve::load_agent_prompt(&args.name, &ctx.config))
        .ok_or_else(|| anyhow::anyhow!("agent not found: {}", args.name))?;
    Ok(GetAgentOutput {
        name: args.name,
        prompt,
    })
}
