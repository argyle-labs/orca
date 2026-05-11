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

pub use orca_tool_trait::{OrcaOp, OrcaToolDef};

pub mod agent_backend;
pub mod agents;
pub mod docker;
pub mod docs;
pub mod engine;
pub mod homeassistant;
pub mod infra;
pub mod json_any;
pub mod meta;
pub mod mgmt;
pub mod orca_auth;
pub mod orca_db;
pub mod orca_lifecycle;
pub mod orca_pki;
pub mod orca_profile;
pub mod plugin_runtime;
pub mod plugins;
pub mod proxmox;
pub mod services;
pub mod spec_registry;
pub mod system;

/// Re-export of the opaque JSON passthrough wrapper — see `json_any` module for policy.
#[allow(clippy::disallowed_types)]
pub use json_any::JsonAny;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "cli")]
pub mod cli;

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
    ( $( $method:ident => $tool:path $([ d: $domain:literal, v: $verb:literal $(, cli: $cli_mode:ident )? ])? ),* $(,)? ) => {
        // (native) ToolRegistry enrollment — drives MCP + REST + /api/tools/<name>.
        #[cfg(feature = "native")]
        pub fn native_register(reg: &mut $crate::__private::ToolRegistry) {
            $( reg.register::<$tool>(); )*
        }

        // (wasm) Per-tool typed methods on OrcaClient.
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

        // OrcaOp + CLI registration for every entry that carries a [d:.., v:..]
        // tag. Default-rendered as JSON; override per-tool by writing a manual
        // register_op! elsewhere AND tagging the entry with `cli: manual`.
        $(
            $(
                $crate::__declare_op!($tool, $domain, $verb $(, $cli_mode)?);
            )?
        )*
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __declare_op {
    // [d:..., v:..., cli: manual] — emit OrcaOp impl only; tool file owns the
    // register_op! call (used for bespoke colored output like engine list).
    ($tool:path, $domain:literal, $verb:literal, manual) => {
        impl $crate::OrcaOp for $tool {
            const DOMAIN: &'static str = $domain;
            const VERB: &'static str = $verb;
        }
    };
    // [d:..., v:..., cli: skip] — emit OrcaOp but no CLI registration (Args
    // type can't derive clap::Args; tool reachable via MCP/REST/WASM only).
    ($tool:path, $domain:literal, $verb:literal, skip) => {
        impl $crate::OrcaOp for $tool {
            const DOMAIN: &'static str = $domain;
            const VERB: &'static str = $verb;
        }
    };
    // Default: emit OrcaOp + register_op! with JSON-pretty output.
    ($tool:path, $domain:literal, $verb:literal) => {
        impl $crate::OrcaOp for $tool {
            const DOMAIN: &'static str = $domain;
            const VERB: &'static str = $verb;
        }
        #[cfg(feature = "cli")]
        const _: () = {
            $crate::register_op! {
                tool: $tool,
                domain: $domain,
                verb: $verb,
                summary: <$tool as $crate::OrcaToolDef>::DESCRIPTION,
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
    // ── Server liveness ─────────────────────────────────────────────────────
    health => meta::ApiHealth [d:"system", v:"health"],

    // ── Auth / credentials (unified across anthropic + github + atlassian) ──
    auth_status => orca_auth::AuthStatus [d:"auth", v:"status"],
    auth_logout => orca_auth::AuthLogout [d:"auth", v:"logout"],
    auth_login  => orca_auth::AuthLogin  [d:"auth", v:"login"],

    // ── orca.db admin ───────────────────────────────────────────────────────
    db_status   => orca_db::DbStatus   [d:"db", v:"status"],
    db_migrate  => orca_db::DbMigrate  [d:"db", v:"migrate"],
    db_up       => orca_db::DbUp       [d:"db", v:"up"],
    db_down     => orca_db::DbDown     [d:"db", v:"down"],

    // ── PKI (orca CA + plugin certs) ────────────────────────────────────────
    pki_ca_init    => orca_pki::PkiCaInit    [d:"pki", v:"ca-init"],
    pki_cert_issue => orca_pki::PkiCertIssue [d:"pki", v:"cert-issue"],
    pki_list       => orca_pki::PkiList      [d:"pki", v:"list"],

    // ── Profiles ────────────────────────────────────────────────────────────
    profile_list    => orca_profile::ProfileList    [d:"profile", v:"list"],
    profile_show    => orca_profile::ProfileShow    [d:"profile", v:"show"],
    profile_current => orca_profile::ProfileCurrent [d:"profile", v:"current"],
    profile_create  => orca_profile::ProfileCreate  [d:"profile", v:"create"],
    profile_delete  => orca_profile::ProfileDelete  [d:"profile", v:"delete"],
    profile_use     => orca_profile::ProfileUse     [d:"profile", v:"use"],
    profile_share   => orca_profile::ProfileShare   [d:"profile", v:"share"],
    profile_unshare => orca_profile::ProfileUnshare [d:"profile", v:"unshare"],
    profile_shares  => orca_profile::ProfileShares  [d:"profile", v:"shares"],

    // ── orca install lifecycle + admin one-shots ────────────────────────────
    system_install      => orca_lifecycle::SystemInstall      [d:"system", v:"install"],
    system_uninstall    => orca_lifecycle::SystemUninstall    [d:"system", v:"uninstall"],
    system_doctor       => orca_lifecycle::SystemDoctor       [d:"system", v:"doctor"],
    system_update_check => orca_lifecycle::SystemUpdateCheck  [d:"system", v:"update-check"],
    system_update_apply => orca_lifecycle::SystemUpdateApply  [d:"system", v:"update-apply"],
    projects_list       => orca_lifecycle::ProjectsList       [d:"projects", v:"list"],
    spec_dump           => orca_lifecycle::SpecDump           [d:"spec", v:"dump"],

    // ── Engine registry (bespoke colored rendering kept in engine.rs) ──────
    engine_list      => engine::EngineList    [d:"engine", v:"list",    cli: manual],
    engine_add       => engine::EngineAdd     [d:"engine", v:"add",     cli: manual],
    engine_remove    => engine::EngineRemove  [d:"engine", v:"remove",  cli: manual],
    engine_enable    => engine::EngineEnable  [d:"engine", v:"enable",  cli: manual],
    engine_disable   => engine::EngineDisable [d:"engine", v:"disable", cli: manual],

    // ── Agent backend ───────────────────────────────────────────────────────
    agent_backend_clear_api_key  => agent_backend::AgentBackendClearApiKey  [d:"agent-backend", v:"clear-key"],
    agent_backend_set_api_key    => agent_backend::AgentBackendSetApiKey    [d:"agent-backend", v:"set-key"],
    agent_backend_api_key_status => agent_backend::AgentBackendApiKeyStatus [d:"agent-backend", v:"key-status"],
    agent_backend_set_mode             => agent_backend::AgentBackendSetMode             [d:"agent-backend", v:"set-mode"],
    agent_backend_override             => agent_backend::AgentBackendOverride            [d:"agent-backend", v:"override"],
    agent_backend_use_server_anthropic => agent_backend::AgentBackendUseServerAnthropic  [d:"agent-backend", v:"use-server-anthropic"],
    agent_backend_status               => agent_backend::AgentBackendStatus              [d:"agent-backend", v:"status"],

    // ── Agents ──────────────────────────────────────────────────────────────
    list_agents  => agents::ListAgents  [d:"agents", v:"list"],
    get_agent    => agents::GetAgent    [d:"agents", v:"get"],
    get_config   => agents::GetConfig   [d:"agents", v:"get-config"],
    get_context  => agents::GetContext  [d:"agents", v:"get-context"],
    search_logs  => agents::SearchLogs  [d:"agents", v:"search-logs"],

    // ── Docs ────────────────────────────────────────────────────────────────
    list_roots     => docs::ListRoots    [d:"docs", v:"list-roots"],
    get_tree       => docs::GetTree      [d:"docs", v:"tree"],
    get_full_tree  => docs::GetFullTree  [d:"docs", v:"full-tree"],
    read_doc       => docs::ReadDoc      [d:"docs", v:"read"],
    search_docs    => docs::SearchDocs   [d:"docs", v:"search"],
    list_commands  => docs::ListCommands [d:"docs", v:"list-commands"],

    // ── Infra ───────────────────────────────────────────────────────────────
    list_services    => infra::ListServices    [d:"infra", v:"services"],
    get_service_logs => infra::GetServiceLogs  [d:"infra", v:"service-logs"],
    run_tests        => infra::RunTests        [d:"infra", v:"run-tests"],

    // ── Plugins ─────────────────────────────────────────────────────────────
    list_plugins        => plugins::ListPlugins       [d:"plugin", v:"list"],
    add_plugin          => plugins::AddPlugin         [d:"plugin", v:"add"],
    remove_plugin       => plugins::RemovePlugin      [d:"plugin", v:"remove"],
    enable_plugin       => plugins::EnablePlugin      [d:"plugin", v:"enable"],
    disable_plugin      => plugins::DisablePlugin     [d:"plugin", v:"disable"],
    list_plugin_creds   => plugins::ListPluginCreds   [d:"plugin", v:"list-creds"],
    set_plugin_cred     => plugins::SetPluginCred     [d:"plugin", v:"set-cred"],
    remove_plugin_cred  => plugins::RemovePluginCred  [d:"plugin", v:"remove-cred"],
    sync_plugin_creds   => plugins::SyncPluginCreds   [d:"plugin", v:"sync-creds"],

    // ── MCP federation (run takes opaque JSON → CLI-skipped) ────────────────
    list_mcp_tools       => mgmt::ListMcpTools  [d:"mcp-federation", v:"list-tools"],
    run_mcp_tool         => mgmt::RunMcpTool    [d:"mcp-federation", v:"run", cli: skip],

    // ── Schema view ─────────────────────────────────────────────────────────
    get_schema           => mgmt::GetSchema         [d:"schema-view", v:"get"],
    get_schema_domains   => mgmt::GetSchemaDomains  [d:"schema-view", v:"list-domains"],

    // ── Plugin runtime KV (set takes opaque Value → CLI-skipped) ────────────
    get_plugin_data => plugin_runtime::GetPluginData [d:"plugin-data", v:"get"],
    set_plugin_data => plugin_runtime::SetPluginData [d:"plugin-data", v:"set", cli: skip],

    // ── MCP servers + mappings ──────────────────────────────────────────────
    list_mcp_servers     => mgmt::ListMcpServers      [d:"mcp", v:"list"],
    add_mcp_server       => mgmt::AddMcpServer        [d:"mcp", v:"add"],
    remove_mcp_server    => mgmt::RemoveMcpServer     [d:"mcp", v:"remove"],
    map_tool             => mgmt::MapTool             [d:"mcp", v:"map"],
    unmap_tool           => mgmt::UnmapTool           [d:"mcp", v:"unmap"],
    sync_tools           => mgmt::SyncTools           [d:"mcp", v:"sync"],
    list_tool_mappings   => mgmt::ListToolMappings    [d:"mcp", v:"list-mappings"],

    // ── Schema databases ────────────────────────────────────────────────────
    list_schemas         => mgmt::ListSchemas   [d:"schema", v:"list"],
    add_schema           => mgmt::AddSchema     [d:"schema", v:"add"],
    remove_schema        => mgmt::RemoveSchema  [d:"schema", v:"remove"],

    // ── Docker runtimes ─────────────────────────────────────────────────────
    list_docker_runtimes  => mgmt::ListDockerRuntimes   [d:"docker-runtime", v:"list"],
    add_docker_runtime    => mgmt::AddDockerRuntime     [d:"docker-runtime", v:"add"],
    remove_docker_runtime => mgmt::RemoveDockerRuntime  [d:"docker-runtime", v:"remove"],

    // ── Doc roots + ignore patterns ─────────────────────────────────────────
    list_doc_roots             => mgmt::ListDocRoots            [d:"doc-root", v:"list"],
    add_doc_root               => mgmt::AddDocRoot              [d:"doc-root", v:"add"],
    remove_doc_root            => mgmt::RemoveDocRoot           [d:"doc-root", v:"remove"],
    list_doc_ignore_patterns   => mgmt::ListDocIgnorePatterns   [d:"doc-pattern", v:"list"],
    add_doc_ignore_pattern     => mgmt::AddDocIgnorePattern     [d:"doc-pattern", v:"add"],
    remove_doc_ignore_pattern  => mgmt::RemoveDocIgnorePattern  [d:"doc-pattern", v:"remove"],

    // ── Proxmox endpoints (registry; live actions are in proxmox::*) ────────
    list_proxmox_endpoints   => mgmt::ListProxmoxEndpoints   [d:"proxmox-endpoint", v:"list"],
    add_proxmox_endpoint     => mgmt::AddProxmoxEndpoint     [d:"proxmox-endpoint", v:"add"],
    remove_proxmox_endpoint  => mgmt::RemoveProxmoxEndpoint  [d:"proxmox-endpoint", v:"remove"],

    // ── Home Assistant endpoints (registry) ─────────────────────────────────
    list_home_assistant_endpoints  => mgmt::ListHomeAssistantEndpoints   [d:"ha-endpoint", v:"list"],
    add_home_assistant_endpoint    => mgmt::AddHomeAssistantEndpoint     [d:"ha-endpoint", v:"add"],
    remove_home_assistant_endpoint => mgmt::RemoveHomeAssistantEndpoint  [d:"ha-endpoint", v:"remove"],

    // ── Home Assistant (live integration) ───────────────────────────────────
    home_assistant_entity_list     => homeassistant::HaEntityList      [d:"ha", v:"entity-list"],
    home_assistant_entity_state    => homeassistant::HaEntityState     [d:"ha", v:"entity-state"],
    home_assistant_automation_list => homeassistant::HaAutomationList  [d:"ha", v:"automation-list"],
    home_assistant_service_call    => homeassistant::HaServiceCall     [d:"ha", v:"service-call"],

    // ── Proxmox (live integration) ──────────────────────────────────────────
    proxmox_list_nodes       => proxmox::ProxmoxListNodes        [d:"proxmox", v:"nodes"],
    proxmox_list_vms         => proxmox::ProxmoxListVms          [d:"proxmox", v:"vms"],
    proxmox_list_containers  => proxmox::ProxmoxListContainers   [d:"proxmox", v:"containers"],
    proxmox_vm_action        => proxmox::ProxmoxVmAction         [d:"proxmox", v:"vm-action"],
    proxmox_container_action => proxmox::ProxmoxContainerAction  [d:"proxmox", v:"container-action"],

    // ── Docker engine + compose ─────────────────────────────────────────────
    get_docker_engine    => docker::GetDockerEngine    [d:"docker", v:"engine"],
    start_docker_engine  => docker::StartDockerEngine  [d:"docker", v:"engine-start"],
    get_docker_services  => docker::GetDockerServices  [d:"docker", v:"services"],
    run_docker_action    => docker::RunDockerAction    [d:"docker", v:"action"],
    get_logs             => docker::GetLogs            [d:"docker", v:"logs"],
    get_log_services     => docker::GetLogServices     [d:"docker", v:"log-services"],

    // ── Spec registry (proxy_graphql takes opaque vars → CLI-skipped) ───────
    list_specs            => spec_registry::ListSpecs            [d:"spec", v:"list"],
    list_db_specs         => spec_registry::ListDbSpecs          [d:"spec", v:"list-db"],
    register_spec         => spec_registry::RegisterSpec         [d:"spec", v:"register"],
    refresh_spec          => spec_registry::RefreshSpec          [d:"spec", v:"refresh"],
    unregister_spec       => spec_registry::UnregisterSpec       [d:"spec", v:"unregister"],
    sync_mcp_specs        => spec_registry::SyncMcpSpecs         [d:"spec", v:"sync-mcp"],
    get_spec_graphql_info => spec_registry::GetSpecGraphqlInfo   [d:"spec", v:"graphql-info"],
    proxy_graphql         => spec_registry::ProxyGraphql         [d:"spec", v:"proxy-graphql", cli: skip],

    // ── System / orca install lifecycle ─────────────────────────────────────
    system_status => system::SystemStatus  [d:"system", v:"status"],
    system_action => system::SystemAction  [d:"system", v:"action"],
}
