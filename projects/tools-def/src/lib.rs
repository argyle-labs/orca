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

pub use orca_tool_trait::OrcaToolDef;

pub mod agent_backend;
pub mod agents;
pub mod docs;
pub mod engine;
pub mod homeassistant;
pub mod infra;
pub mod json_any;
pub mod meta;
pub mod mgmt;
pub mod plugin_runtime;
pub mod plugins;
pub mod proxmox;
pub mod services;

pub use json_any::JsonAny;

#[cfg(feature = "wasm")]
pub mod wasm;

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
                    pub async fn $method(
                        &self,
                        args: <$tool as OrcaToolDef>::Args,
                    ) -> Result<<$tool as OrcaToolDef>::Output, JsValue> {
                        self.call_tool_typed::<$tool>(args).await
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

    // Agent backend — mode/override/status (service-trait dispatch)
    agent_backend_set_mode             => agent_backend::AgentBackendSetMode,
    agent_backend_override             => agent_backend::AgentBackendOverride,
    agent_backend_use_server_anthropic => agent_backend::AgentBackendUseServerAnthropic,
    agent_backend_status               => agent_backend::AgentBackendStatus,

    // Agents — listing, prompts, config docs, project memory, log search
    list_agents  => agents::ListAgents,
    get_agent    => agents::GetAgent,
    get_config   => agents::GetConfig,
    get_context  => agents::GetContext,
    search_logs  => agents::SearchLogs,

    // Docs — roots, tree, read, search, commands
    list_roots     => docs::ListRoots,
    get_tree       => docs::GetTree,
    read_doc       => docs::ReadDoc,
    search_docs    => docs::SearchDocs,
    list_commands  => docs::ListCommands,

    // Infra — docker compose services, logs, tests
    list_services    => infra::ListServices,
    get_service_logs => infra::GetServiceLogs,
    run_tests        => infra::RunTests,

    // Plugins — registry + credentials
    list_plugins        => plugins::ListPlugins,
    add_plugin          => plugins::AddPlugin,
    remove_plugin       => plugins::RemovePlugin,
    enable_plugin       => plugins::EnablePlugin,
    disable_plugin      => plugins::DisablePlugin,
    list_plugin_creds   => plugins::ListPluginCreds,
    set_plugin_cred     => plugins::SetPluginCred,
    remove_plugin_cred  => plugins::RemovePluginCred,
    sync_plugin_creds   => plugins::SyncPluginCreds,

    // Mgmt — MCP federation (live tools + run)
    list_mcp_tools       => mgmt::ListMcpTools,
    run_mcp_tool         => mgmt::RunMcpTool,
    // Mgmt — schema view
    get_schema           => mgmt::GetSchema,
    get_schema_domains   => mgmt::GetSchemaDomains,

    // Plugin runtime KV
    get_plugin_data => plugin_runtime::GetPluginData,
    set_plugin_data => plugin_runtime::SetPluginData,

    // Mgmt — MCP servers + mappings
    list_mcp_servers     => mgmt::ListMcpServers,
    add_mcp_server       => mgmt::AddMcpServer,
    remove_mcp_server    => mgmt::RemoveMcpServer,
    map_tool             => mgmt::MapTool,
    unmap_tool           => mgmt::UnmapTool,
    sync_tools           => mgmt::SyncTools,
    list_tool_mappings   => mgmt::ListToolMappings,
    // Mgmt — schema databases
    list_schemas         => mgmt::ListSchemas,
    add_schema           => mgmt::AddSchema,
    remove_schema        => mgmt::RemoveSchema,
    // Mgmt — docker runtimes
    list_docker_runtimes  => mgmt::ListDockerRuntimes,
    add_docker_runtime    => mgmt::AddDockerRuntime,
    remove_docker_runtime => mgmt::RemoveDockerRuntime,
    // Mgmt — doc roots + ignore patterns
    list_doc_roots             => mgmt::ListDocRoots,
    add_doc_root               => mgmt::AddDocRoot,
    remove_doc_root            => mgmt::RemoveDocRoot,
    list_doc_ignore_patterns   => mgmt::ListDocIgnorePatterns,
    add_doc_ignore_pattern     => mgmt::AddDocIgnorePattern,
    remove_doc_ignore_pattern  => mgmt::RemoveDocIgnorePattern,
    // Mgmt — proxmox endpoints
    list_proxmox_endpoints   => mgmt::ListProxmoxEndpoints,
    add_proxmox_endpoint     => mgmt::AddProxmoxEndpoint,
    remove_proxmox_endpoint  => mgmt::RemoveProxmoxEndpoint,
    // Mgmt — home assistant endpoints
    list_home_assistant_endpoints  => mgmt::ListHomeAssistantEndpoints,
    add_home_assistant_endpoint    => mgmt::AddHomeAssistantEndpoint,
    remove_home_assistant_endpoint => mgmt::RemoveHomeAssistantEndpoint,

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
