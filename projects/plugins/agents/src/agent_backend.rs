//! Agent-backend tools — manage the LLM resolution config (mode, per-agent
//! overrides, server-side Anthropic toggle, encrypted API key). Tool bodies
//! call `llm::resolve` and `db::settings` directly — no service-trait
//! indirection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use derive::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearArgs {}

/// Outcome of a mutation against the encrypted API-key slot.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApiKeyMutationResult {
    pub present: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetArgs {
    pub key: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetModeArgs {
    /// "local" | "claude" | "hybrid"
    pub mode: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetModeResult {
    pub mode: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct OverrideArgs {
    pub agent: String,
    /// "local" | "claude" | "clear" (clear removes the override)
    pub backend: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct OverrideResult {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub cleared: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UseServerAnthropicArgs {
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UseServerAnthropicResult {
    pub enabled: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendStatusArgs {}

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

/// [MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.
#[orca_tool(domain = "system.agent.backend", verb = "clear-key")]
async fn agent_backend_clear_api_key(
    _args: ClearArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ApiKeyMutationResult> {
    let conn = db::open_default()?;
    let removed = db::settings::secret_delete(&conn, "anthropic_api_key")?;
    Ok(ApiKeyMutationResult {
        present: false,
        message: if removed {
            "removed Anthropic API key from orca DB".to_string()
        } else {
            "no Anthropic API key was stored".to_string()
        },
        masked: None,
    })
}

/// [MUTATES STATE] Store an Anthropic API key in the encrypted orca DB.
#[orca_tool(domain = "system.agent.backend", verb = "set-key")]
async fn agent_backend_set_api_key(
    args: SetArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ApiKeyMutationResult> {
    if args.key.trim().is_empty() {
        anyhow::bail!("key must not be empty");
    }
    let conn = db::open_default()?;
    db::settings::secret_set(&conn, "anthropic_api_key", &args.key)?;
    let masked = db::settings::mask_key(&args.key);
    Ok(ApiKeyMutationResult {
        present: true,
        message: format!("stored Anthropic API key in encrypted orca DB ({masked})"),
        masked: Some(masked),
    })
}

/// [MUTATES STATE] Set the global agent backend mode (local | claude | hybrid).
#[orca_tool(domain = "system.agent.backend", verb = "set-mode")]
async fn agent_backend_set_mode(
    args: SetModeArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SetModeResult> {
    let parsed = llm::resolve::Mode::parse(&args.mode)?;
    llm::resolve::set_mode(parsed)?;
    Ok(SetModeResult {
        mode: parsed.as_str().to_string(),
    })
}

/// [MUTATES STATE] Set, change, or clear a per-agent backend override.
#[orca_tool(domain = "system.agent.backend", verb = "override")]
async fn agent_backend_override(
    args: OverrideArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<OverrideResult> {
    if args.backend == "clear" {
        let removed = llm::resolve::clear_override(&args.agent)?;
        return Ok(OverrideResult {
            agent: args.agent,
            backend: None,
            cleared: removed,
        });
    }
    let exists = crate::embedded::list_embedded_agents()
        .iter()
        .any(|(name, _)| name == &args.agent);
    if !exists {
        anyhow::bail!("unknown agent: {}", args.agent);
    }
    llm::resolve::set_override(&args.agent, &args.backend)?;
    Ok(OverrideResult {
        agent: args.agent,
        backend: Some(args.backend),
        cleared: false,
    })
}

/// [MUTATES STATE] Toggle whether the orca server makes Anthropic API calls directly.
#[orca_tool(domain = "system.agent.backend", verb = "use-server-anthropic")]
async fn agent_backend_use_server_anthropic(
    args: UseServerAnthropicArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<UseServerAnthropicResult> {
    llm::resolve::set_use_server_anthropic(args.enabled)?;
    Ok(UseServerAnthropicResult {
        enabled: args.enabled,
    })
}

/// Show the current agent backend configuration.
#[orca_tool(domain = "system.agent.backend", verb = "detail")]
async fn agent_backend_detail(
    _args: AgentBackendStatusArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
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
