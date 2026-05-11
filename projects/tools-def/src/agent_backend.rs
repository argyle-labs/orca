//! Agent-backend API-key tools — defs + native impls in one file.
//!
//! Wasm-safe metadata is always compiled; the DB-backed `run` impls require
//! the `native` feature.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::OrcaToolDef;

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AgentBackendClearApiKey;
impl OrcaToolDef for AgentBackendClearApiKey {
    const NAME: &'static str = "agent_backend_clear_api_key";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove the stored Anthropic API key from the encrypted orca DB.";
    type Args = ClearArgs;
    type Output = ApiKeyMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetArgs {
    /// Anthropic API key (sk-ant-...)
    pub key: String,
}

pub struct AgentBackendSetApiKey;
impl OrcaToolDef for AgentBackendSetApiKey {
    const NAME: &'static str = "agent_backend_set_api_key";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Store an Anthropic API key in the encrypted orca DB \
         (settings table, key 'secrets.anthropic_api_key'). The DB is SQLCipher-encrypted \
         at rest. Required for server-side Anthropic calls.";
    type Args = SetArgs;
    type Output = ApiKeyMutationResult;
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct StatusArgs {}

pub struct AgentBackendApiKeyStatus;
impl OrcaToolDef for AgentBackendApiKeyStatus {
    const NAME: &'static str = "agent_backend_api_key_status";
    const DESCRIPTION: &'static str = "Report whether an Anthropic API key is stored in the encrypted orca DB. \
         Never echoes the raw key — only a masked preview.";
    type Args = StatusArgs;
    type Output = ApiKeyStatus;
}

// ── agent_backend_set_mode ──────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AgentBackendSetMode;
impl OrcaToolDef for AgentBackendSetMode {
    const NAME: &'static str = "agent_backend_set_mode";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Set the global agent backend mode. \
         local = always LM Studio. claude = always route to Claude (server-side if enabled, \
         else delegate to caller). hybrid = check per-agent override; default is Claude \
         when no override is set.";
    type Args = SetModeArgs;
    type Output = SetModeResult;
}

// ── agent_backend_override ──────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AgentBackendOverride;
impl OrcaToolDef for AgentBackendOverride {
    const NAME: &'static str = "agent_backend_override";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Set, change, or clear a per-agent backend override \
         (only consulted in hybrid mode). backend=clear deletes the override.";
    type Args = OverrideArgs;
    type Output = OverrideResult;
}

// ── agent_backend_use_server_anthropic ──────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AgentBackendUseServerAnthropic;
impl OrcaToolDef for AgentBackendUseServerAnthropic {
    const NAME: &'static str = "agent_backend_use_server_anthropic";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Toggle whether the orca server makes Anthropic API calls directly \
         when the resolver picks Claude. When false (default), Claude-routed agents return \
         a delegate-to-claude-code envelope instead. Requires a stored API key when true.";
    type Args = UseServerAnthropicArgs;
    type Output = UseServerAnthropicResult;
}

// ── agent_backend_status ────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

pub struct AgentBackendStatus;
impl OrcaToolDef for AgentBackendStatus {
    const NAME: &'static str = "agent_backend_status";
    const DESCRIPTION: &'static str = "Show the current agent backend configuration: mode (local|claude|hybrid), \
         per-agent overrides, and whether server-side Anthropic calls are enabled.";
    type Args = AgentBackendStatusArgs;
    type Output = AgentBackendStatusOutput;
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::agent_backend::AgentBackendService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_db as db;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn AgentBackendService>> {
        ctx.service::<Arc<dyn AgentBackendService>>()
    }

    #[async_trait]
    impl OrcaTool for AgentBackendClearApiKey {
        async fn run(_args: ClearArgs, _ctx: &ToolCtx) -> Result<ApiKeyMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for AgentBackendSetApiKey {
        async fn run(args: SetArgs, _ctx: &ToolCtx) -> Result<ApiKeyMutationResult> {
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
    }

    #[async_trait]
    impl OrcaTool for AgentBackendApiKeyStatus {
        async fn run(_args: StatusArgs, _ctx: &ToolCtx) -> Result<ApiKeyStatus> {
            let conn = db::open_default()?;
            match db::settings::secret_get(&conn, "anthropic_api_key")? {
                Some(k) => Ok(ApiKeyStatus {
                    present: true,
                    masked: Some(db::settings::mask_key(&k)),
                }),
                None => Ok(ApiKeyStatus {
                    present: false,
                    masked: None,
                }),
            }
        }
    }

    #[async_trait]
    impl OrcaTool for AgentBackendSetMode {
        async fn run(args: SetModeArgs, ctx: &ToolCtx) -> Result<SetModeResult> {
            let mode = svc(ctx)?.set_mode(&args.mode).await?;
            Ok(SetModeResult { mode })
        }
    }

    #[async_trait]
    impl OrcaTool for AgentBackendOverride {
        async fn run(args: OverrideArgs, ctx: &ToolCtx) -> Result<OverrideResult> {
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
    }

    #[async_trait]
    impl OrcaTool for AgentBackendUseServerAnthropic {
        async fn run(
            args: UseServerAnthropicArgs,
            ctx: &ToolCtx,
        ) -> Result<UseServerAnthropicResult> {
            svc(ctx)?.set_use_server_anthropic(args.enabled).await?;
            Ok(UseServerAnthropicResult {
                enabled: args.enabled,
            })
        }
    }

    #[async_trait]
    impl OrcaTool for AgentBackendStatus {
        async fn run(
            _args: AgentBackendStatusArgs,
            ctx: &ToolCtx,
        ) -> Result<AgentBackendStatusOutput> {
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
    }
}
