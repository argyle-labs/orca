//! Server-coupled agent_backend tools — 4 tools, one file each.
//!
//! The 3 API-key tools (clear, set, status) have moved to
//! `orca-tools/src/agent_backend/` because they're pure DB ops. The remaining
//! tools here still reach into server internals (config + AgentBackend) and
//! stay until a service-trait abstraction lets them move.

mod backend_override;
mod set_mode;
mod status;
mod use_server_anthropic;

pub use backend_override::AgentBackendOverride;
pub use set_mode::AgentBackendSetMode;
pub use status::AgentBackendStatus;
pub use use_server_anthropic::AgentBackendUseServerAnthropic;

/// Register all server-coupled agent_backend tools into a registry.
pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<AgentBackendStatus>()
        .register::<AgentBackendSetMode>()
        .register::<AgentBackendOverride>()
        .register::<AgentBackendUseServerAnthropic>();
}
