//! Agent-backend API-key tools — defs + native impls in one file.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[cfg(feature = "native")]
use orca_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

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

/// [MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.
#[orca_tool(domain = "system.agent.backend", verb = "clear-key")]
async fn agent_backend_clear_api_key(
    _args: ClearArgs,
    _ctx: &orca_contract::ToolCtx,
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
    _ctx: &orca_contract::ToolCtx,
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
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SetModeResult> {
    let mode = ctx
        .service::<Arc<dyn AgentBackendService>>()?
        .set_mode(&args.mode)
        .await?;
    Ok(SetModeResult { mode })
}

/// [MUTATES STATE] Set, change, or clear a per-agent backend override (only consulted in hybrid mode). backend=clear deletes the override.
#[orca_tool(domain = "system.agent.backend", verb = "override")]
async fn agent_backend_override(
    args: OverrideArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<OverrideResult> {
    let s = ctx.service::<Arc<dyn AgentBackendService>>()?;
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
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<UseServerAnthropicResult> {
    ctx.service::<Arc<dyn AgentBackendService>>()?
        .set_use_server_anthropic(args.enabled)
        .await?;
    Ok(UseServerAnthropicResult {
        enabled: args.enabled,
    })
}

/// Show the current agent backend configuration: mode (local|claude|hybrid), per-agent overrides, whether server-side Anthropic calls are enabled, and a masked preview of the stored API key (when present).
#[orca_tool(domain = "system.agent.backend", verb = "detail")]
async fn agent_backend_detail(
    _args: AgentBackendStatusArgs,
    ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<AgentBackendStatusOutput> {
    let s = ctx.service::<Arc<dyn AgentBackendService>>()?;
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

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait AgentBackendService: Send + Sync {
    /// Current global mode as its canonical string ("local"|"claude"|"hybrid").
    async fn current_mode(&self) -> Result<String>;

    /// Set the global mode. Accepts the same strings as `current_mode`
    /// returns. Returns the parsed canonical mode string for echo.
    async fn set_mode(&self, mode: &str) -> Result<String>;

    async fn use_server_anthropic(&self) -> Result<bool>;
    async fn set_use_server_anthropic(&self, enabled: bool) -> Result<()>;

    /// All per-agent overrides as (agent, backend) pairs.
    async fn list_overrides(&self) -> Result<Vec<(String, String)>>;

    async fn set_override(&self, agent: &str, backend: &str) -> Result<()>;

    /// Remove a per-agent override. Returns `true` if one was present.
    async fn clear_override(&self, agent: &str) -> Result<bool>;

    /// Validate an agent name against the embedded agent set.
    async fn agent_exists(&self, agent: &str) -> Result<bool>;

    /// Whether an Anthropic API key is currently stored in the encrypted DB.
    async fn api_key_present(&self) -> Result<bool>;
}

/// Embedder hook — implemented by hosts (orca-server, orca-app-kit) to
/// yield their concrete `AgentBackendService` impl into `ToolCtx`. See
/// `services::mod` doc for the convention.
pub trait ProvideAgentBackend {
    fn agent_backend(&self) -> std::sync::Arc<dyn AgentBackendService>;
}

/// Register an `AgentBackendService` into `ToolCtx` from any embedder that
/// implements `ProvideAgentBackend`.
pub fn register_agent_backend(ctx: &mut orca_contract::ToolCtx, p: &impl ProvideAgentBackend) {
    ctx.register_service(p.agent_backend());
}
