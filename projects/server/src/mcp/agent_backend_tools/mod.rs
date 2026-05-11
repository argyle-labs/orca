//! Server-coupled agent_backend tools — 4 tools, one file each.
//!
//! The 3 API-key tools (clear, set, status) have moved to tools-def because
//! they're pure DB ops. These 4 stay until a service-trait abstraction lets
//! them migrate too.

mod backend_override;
mod set_mode;
mod status;
mod use_server_anthropic;

pub use backend_override::AgentBackendOverride;
pub use set_mode::AgentBackendSetMode;
pub use status::AgentBackendStatus;
pub use use_server_anthropic::AgentBackendUseServerAnthropic;

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<AgentBackendStatus>()
        .register::<AgentBackendSetMode>()
        .register::<AgentBackendOverride>()
        .register::<AgentBackendUseServerAnthropic>();
}
