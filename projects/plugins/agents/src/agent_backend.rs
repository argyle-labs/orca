//! Agent-backend tools — manage the LLM resolution config (mode, per-agent
//! overrides, server-side Anthropic toggle, encrypted API key). Tool bodies
//! call `llm::resolve` and `db::settings` directly — no service-trait
//! indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendOverrideEntry {
    pub agent: String,
    pub backend: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendStatusOutput {
    pub mode: String,
    pub use_server_anthropic: bool,
    pub api_key_in_db: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_masked: Option<String>,
    pub overrides: Vec<AgentBackendOverrideEntry>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct AgentBackendDetailArgs {}

/// Show the current agent backend configuration.
#[orca_tool(domain = "agent.backend", verb = "detail")]
async fn agent_backend_detail(
    _args: AgentBackendDetailArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
    read_status()
}

/// [MUTATES STATE] Update one or more agent-backend settings: mode, API key,
/// per-agent override, server-side Anthropic toggle. All fields optional;
/// only the supplied fields are applied. Returns the fresh full status.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default)]
pub struct AgentBackendUpdateArgs {
    /// Global backend mode: "local" | "claude" | "hybrid".
    #[arg(long)]
    pub mode: Option<String>,
    /// Anthropic API key to store in the encrypted orca DB.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Remove the stored Anthropic API key.
    #[arg(long)]
    pub clear_api_key: bool,
    /// Per-agent override: set with `--override-agent NAME --override-backend
    /// {local|claude}`. Clear with `--override-backend clear`.
    #[arg(long)]
    pub override_agent: Option<String>,
    #[arg(long)]
    pub override_backend: Option<String>,
    /// Toggle whether the orca server makes Anthropic API calls directly.
    #[arg(long)]
    pub use_server_anthropic: Option<bool>,
}

#[orca_tool(domain = "agent.backend", verb = "update")]
async fn agent_backend_update(
    args: AgentBackendUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
    if let Some(mode) = args.mode.as_deref() {
        let parsed = llm::resolve::Mode::parse(mode)?;
        llm::resolve::set_mode(parsed)?;
    }

    if args.clear_api_key {
        let conn = db::open_default()?;
        db::settings::secret_delete(&conn, "anthropic_api_key")?;
    }
    if let Some(key) = args.api_key.as_deref() {
        if key.trim().is_empty() {
            anyhow::bail!("api_key must not be empty");
        }
        let conn = db::open_default()?;
        db::settings::secret_set(&conn, "anthropic_api_key", key)?;
    }

    match (
        args.override_agent.as_deref(),
        args.override_backend.as_deref(),
    ) {
        (Some(agent), Some("clear")) => {
            llm::resolve::clear_override(agent)?;
        }
        (Some(agent), Some(backend)) => {
            let exists = crate::embedded::list_embedded_agents()
                .iter()
                .any(|(name, _)| name == agent);
            if !exists {
                anyhow::bail!("unknown agent: {agent}");
            }
            llm::resolve::set_override(agent, backend)?;
        }
        (None, None) => {}
        _ => anyhow::bail!("override requires both --override-agent and --override-backend"),
    }

    if let Some(enabled) = args.use_server_anthropic {
        llm::resolve::set_use_server_anthropic(enabled)?;
    }

    read_status()
}

fn read_status() -> anyhow::Result<AgentBackendStatusOutput> {
    let mode = llm::resolve::current_mode()?.as_str().to_string();
    let use_server_anthropic = llm::resolve::use_server_anthropic()?;
    let conn = db::open_default()?;
    let stored = db::settings::secret_get(&conn, "anthropic_api_key")?;
    let api_key_in_db = stored.is_some();
    let api_key_masked = stored.as_deref().map(db::settings::mask_key);
    let overrides = llm::resolve::list_overrides()?
        .into_iter()
        .map(|(agent, backend)| AgentBackendOverrideEntry { agent, backend })
        .collect();
    Ok(AgentBackendStatusOutput {
        mode,
        use_server_anthropic,
        api_key_in_db,
        api_key_masked,
        overrides,
    })
}
