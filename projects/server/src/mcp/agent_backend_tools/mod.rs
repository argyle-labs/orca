//! agent_backend OrcaTool impls — 7 tools, one file each.
//! Registered into ToolRegistry in mcp/mod.rs.

mod api_key_clear;
mod api_key_set;
mod api_key_status;
mod backend_override;
mod set_mode;
mod status;
mod use_server_anthropic;

pub use api_key_clear::AgentBackendClearApiKey;
pub use api_key_set::AgentBackendSetApiKey;
pub use api_key_status::AgentBackendApiKeyStatus;
pub use backend_override::AgentBackendOverride;
pub use set_mode::AgentBackendSetMode;
pub use status::AgentBackendStatus;
pub use use_server_anthropic::AgentBackendUseServerAnthropic;

/// Register all agent_backend tools into a registry.
pub fn register(reg: &mut tool::ToolRegistry) {
    reg.register::<AgentBackendStatus>()
        .register::<AgentBackendSetMode>()
        .register::<AgentBackendOverride>()
        .register::<AgentBackendUseServerAnthropic>()
        .register::<AgentBackendSetApiKey>()
        .register::<AgentBackendClearApiKey>()
        .register::<AgentBackendApiKeyStatus>();
}
