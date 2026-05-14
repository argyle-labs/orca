//! Agent-backend API-key tools — defs + native impls in one file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::orca_tool;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ClearArgs {}

/// Outcome of a mutation against the encrypted API-key slot.
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

/// Whether a stored API key exists.
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ApiKeyStatus {
    pub present: bool,
    /// Masked preview if present (e.g. "sk-ant-…ABCD").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub masked: Option<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetArgs {
    /// Anthropic API key (sk-ant-...)
    pub key: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StatusArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetModeArgs {
    /// "local" | "claude" | "hybrid"
    pub mode: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetModeResult {
    /// Canonical mode string after the change.
    pub mode: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct OverrideArgs {
    pub agent: String,
    /// "local" | "claude" | "clear" (clear removes the override)
    pub backend: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct OverrideResult {
    pub agent: String,
    /// Resulting backend after the call. `None` when an override was cleared
    /// (or when no override existed for the agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    pub cleared: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UseServerAnthropicArgs {
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UseServerAnthropicResult {
    pub enabled: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendStatusArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendOverrideEntry {
    pub agent: String,
    pub backend: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct AgentBackendStatusOutput {
    pub mode: String,
    pub use_server_anthropic: bool,
    pub api_key_in_db: bool,
    pub overrides: Vec<AgentBackendOverrideEntry>,
}

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::agent_backend::AgentBackendService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::agent_backend::AgentBackendService>>()
}

/// [MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.
#[orca_tool(domain = "agent-backend", verb = "clear-key")]
async fn agent_backend_clear_api_key(
    _args: ClearArgs,
    _ctx: &orca_utils::tool::ToolCtx,
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
#[orca_tool(domain = "agent-backend", verb = "set-key")]
async fn agent_backend_set_api_key(
    args: SetArgs,
    _ctx: &orca_utils::tool::ToolCtx,
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

/// Report whether an Anthropic API key is stored in the encrypted orca DB. Never echoes the raw key — only a masked preview.
#[orca_tool(domain = "agent-backend", verb = "key-status")]
async fn agent_backend_api_key_status(
    _args: StatusArgs,
    _ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ApiKeyStatus> {
    let conn = orca_db::open_default()?;
    match orca_db::settings::secret_get(&conn, "anthropic_api_key")? {
        Some(k) => Ok(ApiKeyStatus {
            present: true,
            masked: Some(orca_db::settings::mask_key(&k)),
        }),
        None => Ok(ApiKeyStatus {
            present: false,
            masked: None,
        }),
    }
}

/// [MUTATES STATE] Set the global agent backend mode. local = always LM Studio. claude = always route to Claude (server-side if enabled, else delegate to caller). hybrid = check per-agent override; default is Claude when no override is set.
#[orca_tool(domain = "agent-backend", verb = "set-mode")]
async fn agent_backend_set_mode(
    args: SetModeArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SetModeResult> {
    let mode = svc(ctx)?.set_mode(&args.mode).await?;
    Ok(SetModeResult { mode })
}

/// [MUTATES STATE] Set, change, or clear a per-agent backend override (only consulted in hybrid mode). backend=clear deletes the override.
#[orca_tool(domain = "agent-backend", verb = "override")]
async fn agent_backend_override(
    args: OverrideArgs,
    ctx: &orca_utils::tool::ToolCtx,
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
#[orca_tool(domain = "agent-backend", verb = "use-server-anthropic")]
async fn agent_backend_use_server_anthropic(
    args: UseServerAnthropicArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UseServerAnthropicResult> {
    svc(ctx)?.set_use_server_anthropic(args.enabled).await?;
    Ok(UseServerAnthropicResult {
        enabled: args.enabled,
    })
}

/// Show the current agent backend configuration: mode (local|claude|hybrid), per-agent overrides, and whether server-side Anthropic calls are enabled.
#[orca_tool(domain = "agent-backend", verb = "status")]
async fn agent_backend_status(
    _args: AgentBackendStatusArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
    let s = svc(ctx)?;
    let mode = s.current_mode().await?;
    let use_server_anthropic = s.use_server_anthropic().await?;
    let api_key_in_db = s.api_key_present().await?;
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
        overrides,
    })
}
