//! First-party OrcaTool aggregation.
//!
//! Every tool here is exposed automatically on four surfaces:
//!   - **MCP** stdio (`tools/list`, `tools/call`)
//!   - **REST** (`POST /api/tools/<name>`)
//!   - **CLI**  (`orca exec <name>`)
//!   - **WASM client** (`orcaClient.<name>(args)` — when the `wasm` feature
//!     is enabled and the crate is consumed from `projects/client/wasm/`).
//!
//! Where impls live:
//!   - Integration-backed tools (homeassistant, proxmox, …) live in
//!     `orca-integrations/src/<integration>/tool.rs` — next to their `Client`.
//!     They're re-exported here for ergonomic enrollment.
//!   - Registry-style tools that don't wrap a network integration (engine
//!     backends, agent_backend API-key storage) live in this crate.
//!
//! The `orca_tools!{}` macro at the bottom is the single enrollment point.

#[doc(hidden)]
pub mod __private {
    pub use orca_utils::tool::ToolRegistry;
}

pub mod agent_backend;
pub mod engine;

// Re-export integration-owned tools so the enrollment list below stays flat.
pub use orca_integrations::homeassistant::tool as homeassistant;
pub use orca_integrations::proxmox::tool as proxmox;

/// Enroll a set of OrcaTool types as the first-party tool surface.
///
/// Expands to:
///   - `pub fn register_all(reg: &mut ToolRegistry)` — server uses this to
///     populate the runtime registry, which then drives MCP + REST + CLI.
///   - (when `feature = "wasm"`) wasm-bindgen exports — one method per tool
///     on a generated client struct. **Not implemented yet** — placeholder
///     for Phase 3 of the unified-surface plan.
#[macro_export]
macro_rules! orca_tools {
    ( $( $tool:path ),* $(,)? ) => {
        pub fn register_all(reg: &mut $crate::__private::ToolRegistry) {
            $( reg.register::<$tool>(); )*
        }
    };
}

orca_tools! {
    // Engine registry (LM Studio / Ollama)
    engine::EngineList,
    engine::EngineAdd,
    engine::EngineRemove,
    engine::EngineEnable,
    engine::EngineDisable,

    // Agent backend — API-key storage (pure DB)
    agent_backend::AgentBackendClearApiKey,
    agent_backend::AgentBackendSetApiKey,
    agent_backend::AgentBackendApiKeyStatus,

    // Home Assistant (impls live in orca-integrations::homeassistant::tool)
    homeassistant::HaEntityList,
    homeassistant::HaEntityState,
    homeassistant::HaAutomationList,
    homeassistant::HaServiceCall,

    // Proxmox (impls live in orca-integrations::proxmox::tool)
    proxmox::ProxmoxListNodes,
    proxmox::ProxmoxListVms,
    proxmox::ProxmoxListContainers,
    proxmox::ProxmoxVmAction,
    proxmox::ProxmoxContainerAction,
}
