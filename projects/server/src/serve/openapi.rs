use std::sync::OnceLock;

use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::api;
use super::mcp_client::McpPool;

/// Static OpenAPI doc skeleton — info, tags, and shared schemas.
///
/// Paths are NOT listed here. They are registered automatically by
/// `utoipa-axum` when each handler is added to the router via
/// `routes!(handler)`. The `#[utoipa::path]` attribute on the handler
/// is the single source of truth for the route.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "orca API",
        version = "0.1.0",
        description = "orca local dev tool — docs, services, schema, MCP proxy"
    ),
    components(schemas(
        super::api::TreeNode,
        super::api::NodeType,
        super::api::SearchResult,
        super::api::McpToolInfo,
        super::api::McpRunRequest,
        super::api::McpRunResponse,
        super::api::McpContent,
        super::api::DockerService,
        super::api::DockerServicesResponse,
        super::api::DockerActionRequest,
        super::api::DockerActionResponse,
        super::api::Ctx7Response,
        super::api::SchemaResponse,
        super::api::SchemaTab,
        super::api::SchemaTableInfo,
        super::api::SchemaColumn,
        super::api::SchemaForeignKey,
        super::api::SchemaDomain,
        super::api::HealthResponse,
        super::api::HealthCheck,
        super::api::LogService,
        super::api::LogProject,
        super::api::LogServicesResponse,
        super::api::LogsResponse,
        super::api::ErrorResponse,
        super::api::OkResponse,
        super::api::SpecFiles,
        super::api::SpecMeta,
        super::api::SpecQuery,
        super::api::TestRunQuery,
        super::api::TestRunResponse,
        super::api::McpServerInfo,
        super::api::McpServerAddRequest,
        super::api::SchemaDbInfo,
        super::api::SchemaDbAddRequest,
        super::api::DockerRuntimeInfo,
        super::api::DockerRuntimeAddRequest,
        super::api::JiraIssuesQuery,
        super::api::TransitionBody,
        super::api::ConfluenceSearchQuery,
        super::api::RepoInfo,
        super::api::PrQuery,
        super::api::GraphQlInfo,
        super::api::GraphQlOperation,
        super::api::GraphQlField,
        super::api::GraphQlType,
        super::api::GraphQlEnum,
        super::api::SystemStatusResponse,
        super::api::ComponentStatus,
        super::api::MpcStatus,
        super::api::SystemActionResponse,
        super::api::SystemActionRequest,
        super::api::SpecDownloadQuery,
        super::api::GraphqlDownloadQuery,
        super::api::GraphqlProxyRequest,
        super::api::ProgressRequest,
        super::api::ProgressResponse,
        super::api::PdfQuery,
        super::api::SpecRegisterRequest,
        super::api::SpecInfo,
        super::api::PluginInfo,
        super::api::CredInfo,
        super::api::SetCredRequest,
        super::api::PluginDataEntry,
        super::api::SetPluginDataRequest,
    )),
    tags(
        // Public domains — served at /api/openapi/public.json
        (name = "docs",       description = "Orca vault document tree and search [public]"),
        (name = "library",    description = "Library documentation via context7 [public]"),
        // Internal domains — orca local use only
        (name = "mcp",        description = "MCP tool proxy — run any connected MCP server tool"),
        (name = "docker",     description = "Docker Compose service management"),
        (name = "schema",     description = "MySQL schema visualizer"),
        (name = "health",     description = "Rebuy service health checks"),
        (name = "logs",       description = "Docker service log streaming"),
        (name = "specs",      description = "External API spec registry"),
        (name = "tests",      description = "Test suite runner"),
        (name = "jira",       description = "Jira issue management via Atlassian REST API"),
        (name = "confluence", description = "Confluence search via Atlassian REST API"),
        (name = "bitbucket",  description = "Bitbucket repo and PR listing"),
        (name = "github",     description = "GitHub repos, pull requests, and issues"),
        (name = "system",     description = "Orca installation status and install/uninstall actions"),
        (name = "learning",   description = "Learning progress tracking"),
        (name = "plugins",    description = "Plugin registry and credential management"),
    )
)]
pub struct ApiDoc;

/// The fully assembled OpenAPI spec — populated once at router build time
/// from `OpenApiRouter::split_for_parts()`. Read by the spec handlers.
static SPEC: OnceLock<utoipa::openapi::OpenApi> = OnceLock::new();

/// Build the `OpenApiRouter` used by both the live server and offline spec
/// dump. Each handler's `#[utoipa::path]` attribute is the single source of
/// truth — `routes!(handler)` registers the axum route AND the OpenAPI
/// metadata. Multi-method paths combine handlers in one `routes!()` call.
pub(super) fn openapi_router() -> OpenApiRouter<std::sync::Arc<McpPool>> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(api::ping_handler))
        .routes(routes!(api::specs_list_handler))
        .routes(routes!(api::specs_db_list_handler))
        .routes(routes!(api::specs_register_handler))
        .routes(routes!(api::specs_get_public_handler))
        .routes(routes!(api::specs_graphql_info_handler))
        .routes(routes!(api::specs_graphql_proxy_handler))
        .routes(routes!(api::graphql_download_handler))
        .routes(routes!(api::specs_get_graphql_handler))
        .routes(routes!(api::spec_download_handler))
        .routes(routes!(api::specs_refresh_handler))
        .routes(routes!(api::specs_unregister_handler))
        .routes(routes!(api::specs_sync_mcp_handler))
        .routes(routes!(api::specs_get_handler))
        .routes(routes!(api::tree_handler))
        .routes(routes!(api::search_handler))
        .routes(routes!(api::mcp_servers_handler, api::mcp_add_handler))
        .routes(routes!(api::mcp_remove_handler))
        .routes(routes!(
            api::mcp_mappings_list_handler,
            api::mcp_mappings_create_handler
        ))
        .routes(routes!(api::mcp_mappings_delete_handler))
        .routes(routes!(api::mcp_tools_handler))
        .routes(routes!(api::mcp_run_handler))
        .routes(routes!(
            api::docker_runtimes_handler,
            api::docker_runtimes_add_handler
        ))
        .routes(routes!(api::docker_runtimes_remove_handler))
        .routes(routes!(api::docker_engine_handler))
        .routes(routes!(api::docker_engine_start_handler))
        .routes(routes!(api::docker_services_handler))
        .routes(routes!(api::docker_action_handler))
        .routes(routes!(api::ctx7_handler))
        .routes(routes!(api::doc_handler))
        .routes(routes!(
            api::get_progress_handler,
            api::save_progress_handler
        ))
        .routes(routes!(api::schema_handler))
        .routes(routes!(api::schema_domains_handler))
        .routes(routes!(
            api::schema_databases_handler,
            api::schema_databases_add_handler
        ))
        .routes(routes!(api::schema_databases_remove_handler))
        .routes(routes!(api::rebuy_health_handler))
        .routes(routes!(api::log_services_handler))
        .routes(routes!(api::log_fetch_handler))
        .routes(routes!(api::tests_run_handler))
        .routes(routes!(api::repos_handler))
        .routes(routes!(api::prs_handler))
        .routes(routes!(api::jira_issues_handler))
        .routes(routes!(
            api::jira_get_transitions_handler,
            api::jira_transition_handler
        ))
        .routes(routes!(api::confluence_search_handler))
        .routes(routes!(api::github_user_handler))
        .routes(routes!(api::github_repos_handler))
        .routes(routes!(api::github_prs_handler))
        .routes(routes!(api::github_issues_handler))
        .routes(routes!(api::github_orgs_handler))
        .routes(routes!(
            api::plugins_list_handler,
            api::plugin_install_handler
        ))
        .routes(routes!(api::plugin_remove_handler))
        .routes(routes!(api::plugin_enable_handler))
        .routes(routes!(api::plugin_disable_handler))
        .routes(routes!(api::plugin_health_handler))
        .routes(routes!(
            api::plugin_creds_list_handler,
            api::plugin_creds_set_handler
        ))
        .routes(routes!(api::plugin_creds_delete_handler))
        .routes(routes!(api::plugin_creds_sync_handler))
        .routes(routes!(api::plugin_data_list_handler))
        .routes(routes!(
            api::plugin_data_get_handler,
            api::plugin_data_set_handler,
            api::plugin_data_delete_handler
        ))
        .routes(routes!(api::system_status_handler))
        .routes(routes!(api::system_action_handler))
        .routes(routes!(api::fs_browse_handler))
        .routes(routes!(api::pdf_handler))
}

pub(super) fn install_spec(mut spec: utoipa::openapi::OpenApi) {
    spec.info.version = env!("CARGO_PKG_VERSION").to_string();
    let _ = SPEC.set(spec);
}

/// Build the OpenAPI spec on demand without starting the server.
/// Used by the `orca spec dump` CLI command.
fn build_spec() -> utoipa::openapi::OpenApi {
    let (_, mut spec) = openapi_router().split_for_parts();
    spec.info.version = env!("CARGO_PKG_VERSION").to_string();
    spec
}

pub fn orca_spec_json() -> serde_json::Value {
    let spec = SPEC.get().cloned().unwrap_or_else(build_spec);
    let mut value = serde_json::to_value(&spec).unwrap_or_default();
    value["x-orca"] = serde_json::json!({
        "repo": "orca",
        "project": "orca",
        "source": "live"
    });
    value
}

pub async fn openapi_handler() -> impl axum::response::IntoResponse {
    axum::Json(orca_spec_json())
}

pub async fn openapi_public_handler() -> impl axum::response::IntoResponse {
    axum::Json(orca_scanner::filter_orca_public(orca_spec_json()))
}
