//! Agent-backend API-key tools — defs + native impls in one file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearArgs {}

/// Outcome of a mutation against the encrypted API-key slot.
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApiKeyMutationResult {
    /// Whether the slot now holds a key (true after `set`, false after `clear`).
    pub present: bool,
    /// Human-readable summary.
    pub message: String,
    /// Masked preview when a key is now present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked: Option<String>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetArgs {
    /// Anthropic API key (sk-ant-...)
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
    /// Canonical mode string after the change.
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
    /// Resulting backend after the call. `None` when an override was cleared
    /// (or when no override existed for the agent).
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
    /// Masked preview of the stored Anthropic key (e.g. "sk-ant-…ABCD"), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_masked: Option<String>,
    pub overrides: Vec<AgentBackendOverrideEntry>,
}

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::agent_backend::AgentBackendService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::agent_backend::AgentBackendService>>()
}

/// [MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.
#[orca_tool(domain = "system.agent.backend", verb = "clear-key")]
async fn agent_backend_clear_api_key(
    _args: ClearArgs,
    _ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ApiKeyMutationResult> {
    let conn = orca_db::open_default()?;
    let removed = orca_db::settings::secret_delete(&conn, "anthropic_api_key")?;
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

/// [MUTATES STATE] Store an Anthropic API key in the encrypted orca DB (settings table, key 'secrets.anthropic_api_key'). The DB is SQLCipher-encrypted at rest. Required for server-side Anthropic calls.
#[orca_tool(domain = "system.agent.backend", verb = "set-key")]
async fn agent_backend_set_api_key(
    args: SetArgs,
    _ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ApiKeyMutationResult> {
    if args.key.trim().is_empty() {
        anyhow::bail!("key must not be empty");
    }
    let conn = orca_db::open_default()?;
    orca_db::settings::secret_set(&conn, "anthropic_api_key", &args.key)?;
    let masked = orca_db::settings::mask_key(&args.key);
    Ok(ApiKeyMutationResult {
        present: true,
        message: format!("stored Anthropic API key in encrypted orca DB ({masked})"),
        masked: Some(masked),
    })
}

/// [MUTATES STATE] Set the global agent backend mode. local = always LM Studio. claude = always route to Claude (server-side if enabled, else delegate to caller). hybrid = check per-agent override; default is Claude when no override is set.
#[orca_tool(domain = "system.agent.backend", verb = "set-mode")]
async fn agent_backend_set_mode(
    args: SetModeArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SetModeResult> {
    let mode = svc(ctx)?.set_mode(&args.mode).await?;
    Ok(SetModeResult { mode })
}

/// [MUTATES STATE] Set, change, or clear a per-agent backend override (only consulted in hybrid mode). backend=clear deletes the override.
#[orca_tool(domain = "system.agent.backend", verb = "override")]
async fn agent_backend_override(
    args: OverrideArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<OverrideResult> {
    let s = svc(ctx)?;
    if args.backend == "clear" {
        let removed = s.clear_override(&args.agent).await?;
        return Ok(OverrideResult {
            agent: args.agent,
            backend: None,
            cleared: removed,
        });
    }
    if !s.agent_exists(&args.agent).await? {
        anyhow::bail!("unknown agent: {}", args.agent);
    }
    s.set_override(&args.agent, &args.backend).await?;
    Ok(OverrideResult {
        agent: args.agent,
        backend: Some(args.backend),
        cleared: false,
    })
}

/// [MUTATES STATE] Toggle whether the orca server makes Anthropic API calls directly when the resolver picks Claude. When false (default), Claude-routed agents return a delegate-to-claude-code envelope instead. Requires a stored API key when true.
#[orca_tool(domain = "system.agent.backend", verb = "use-server-anthropic")]
async fn agent_backend_use_server_anthropic(
    args: UseServerAnthropicArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<UseServerAnthropicResult> {
    svc(ctx)?.set_use_server_anthropic(args.enabled).await?;
    Ok(UseServerAnthropicResult {
        enabled: args.enabled,
    })
}

/// Show the current agent backend configuration: mode (local|claude|hybrid), per-agent overrides, whether server-side Anthropic calls are enabled, and a masked preview of the stored API key (when present).
#[orca_tool(domain = "system.agent.backend", verb = "detail")]
async fn agent_backend_detail(
    _args: AgentBackendStatusArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
    let s = svc(ctx)?;
    let mode = s.current_mode().await?;
    let use_server_anthropic = s.use_server_anthropic().await?;
    let api_key_in_db = s.api_key_present().await?;
    let api_key_masked = if api_key_in_db {
        let conn = orca_db::open_default()?;
        orca_db::settings::secret_get(&conn, "anthropic_api_key")?
            .as_deref()
            .map(orca_db::settings::mask_key)
    } else {
        None
    };
    let overrides = s
        .list_overrides()
        .await?
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
