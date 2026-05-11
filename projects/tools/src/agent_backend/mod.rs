//! Zero-dep `agent_backend` tools — only the API-key tools live here.
//!
//! The other 4 agent_backend tools (status, set_mode, backend_override,
//! use_server_anthropic) still reach into server internals and stay in
//! `projects/server/src/mcp/agent_backend_tools/` until a service-trait
//! abstraction lets them move.

mod api_key_clear;
mod api_key_set;
mod api_key_status;

pub use api_key_clear::AgentBackendClearApiKey;
pub use api_key_set::AgentBackendSetApiKey;
pub use api_key_status::AgentBackendApiKeyStatus;
