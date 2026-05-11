//! Wasm-safe definitions for OrcaTool — metadata + Args/Output types only.
//!
//! Two parallel surfaces emitted from a single canonical tool list:
//!   - `#[cfg(feature = "native")]` `pub fn native_register(reg)` — adds
//!     every tool to a `ToolRegistry` for the MCP + REST + CLI surfaces.
//!   - `#[cfg(feature = "wasm")]` per-tool methods on `wasm::OrcaClient`
//!     emitted by the `declare_tools!` macro, each posting to
//!     `/api/tools/<NAME>` with the args.
//!
//! Adding a new tool ⇒ one line in the `declare_tools!{}` block at the bottom
//! of this file. All four surfaces light up.

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub mod agent_backend;
pub mod engine;
pub mod homeassistant;
pub mod meta;
pub mod proxmox;

#[cfg(feature = "wasm")]
pub mod wasm;

/// Compile-time metadata for an OrcaTool — everything except `run`.
///
/// Implementations of this trait are wasm-safe by construction: they
/// reference only types/consts, never `tokio::fs`, `db::*`, or other
/// native-only code. The WASM client uses these definitions to emit per-tool
/// wasm-bindgen methods that route over HTTP to the corresponding REST
/// endpoint.
pub trait OrcaToolDef: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;

    type Args: DeserializeOwned + JsonSchema + Send;
    type Output: Serialize + JsonSchema + Send + 'static;
}

/// Single source of truth for tool enrollment.
///
/// Each entry is `<method_ident> => <Tool>`. The macro expands to:
///   - (native) `pub fn native_register(reg)` that calls `reg.register::<T>()`
///     for each `T` — drives MCP + REST + CLI.
///   - (wasm) one `pub async fn <method_ident>(args) -> Result<JsValue,JsValue>`
///     on `wasm::OrcaClient` per entry. Method name is the explicit ident
///     (not derived from `NAME`); JS callers see `client.<method_ident>(...)`.
#[macro_export]
macro_rules! declare_tools {
    ( $( $method:ident => $tool:path ),* $(,)? ) => {
        #[cfg(feature = "native")]
        pub fn native_register(reg: &mut $crate::__private::ToolRegistry) {
            $( reg.register::<$tool>(); )*
        }

        #[cfg(feature = "wasm")]
        const _: () = {
            use $crate::wasm::OrcaClient;
            use $crate::OrcaToolDef;
            use wasm_bindgen::prelude::*;

            #[wasm_bindgen]
            impl OrcaClient {
                $(
                    #[wasm_bindgen]
                    pub async fn $method(&self, args: JsValue) -> Result<JsValue, JsValue> {
                        self.call_tool(<$tool as OrcaToolDef>::NAME, args).await
                    }
                )*
            }
        };
    };
}

#[doc(hidden)]
#[cfg(feature = "native")]
pub mod __private {
    pub use orca_utils::tool::ToolRegistry;
}

declare_tools! {
    // Server liveness — canonical four-surface test case
    health => meta::ApiHealth,

    // Engine registry (LM Studio / Ollama)
    engine_list      => engine::EngineList,
    engine_add       => engine::EngineAdd,
    engine_remove    => engine::EngineRemove,
    engine_enable    => engine::EngineEnable,
    engine_disable   => engine::EngineDisable,

    // Agent backend — API-key storage
    agent_backend_clear_api_key  => agent_backend::AgentBackendClearApiKey,
    agent_backend_set_api_key    => agent_backend::AgentBackendSetApiKey,
    agent_backend_api_key_status => agent_backend::AgentBackendApiKeyStatus,

    // Home Assistant
    home_assistant_entity_list     => homeassistant::HaEntityList,
    home_assistant_entity_state    => homeassistant::HaEntityState,
    home_assistant_automation_list => homeassistant::HaAutomationList,
    home_assistant_service_call    => homeassistant::HaServiceCall,

    // Proxmox
    proxmox_list_nodes       => proxmox::ProxmoxListNodes,
    proxmox_list_vms         => proxmox::ProxmoxListVms,
    proxmox_list_containers  => proxmox::ProxmoxListContainers,
    proxmox_vm_action        => proxmox::ProxmoxVmAction,
    proxmox_container_action => proxmox::ProxmoxContainerAction,
}
