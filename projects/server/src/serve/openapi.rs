use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "brain API",
        version = "0.1.0",
        description = "brain local dev tool — docs, services, schema, MCP proxy"
    ),
    paths(
        // public
        super::api::tree_handler,
        super::api::search_handler,
        super::api::doc_handler,
        super::api::ctx7_handler,
        // internal
        super::api::ping_handler,
        super::api::mcp_tools_handler,
        super::api::mcp_run_handler,
        super::api::docker_engine_handler,
        super::api::docker_engine_start_handler,
        super::api::docker_services_handler,
        super::api::docker_action_handler,
        super::api::schema_handler,
        super::api::schema_domains_handler,
        super::api::rebuy_health_handler,
        super::api::log_services_handler,
        super::api::log_fetch_handler,
        super::api::tests_run_handler,
        // registry (internal — brain-local only)
        super::api::specs_list_handler,
        super::api::specs_get_handler,
        super::api::specs_get_public_handler,
        super::api::specs_get_graphql_handler,
        super::api::specs_graphql_info_handler,
        // jira / confluence / bitbucket proxies
        super::api::jira_issues_handler,
        super::api::jira_get_transitions_handler,
        super::api::jira_transition_handler,
        super::api::confluence_search_handler,
        super::api::repos_handler,
        super::api::prs_handler,
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
    )),
    tags(
        // Public domains — served at /api/openapi/public.json
        (name = "docs",       description = "Brain vault document tree and search [public]"),
        (name = "library",    description = "Library documentation via context7 [public]"),
        // Internal domains — brain local use only
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
    )
)]
pub struct ApiDoc;

pub fn brain_spec_json() -> serde_json::Value {
    use utoipa::OpenApi as _;
    let mut doc = ApiDoc::openapi();
    doc.info.version = env!("CARGO_PKG_VERSION").to_string();
    // Stamp with brain identity so consumers know the source.
    let mut spec = serde_json::to_value(&doc).unwrap_or_default();
    spec["x-brain"] = serde_json::json!({
        "repo": "brain",
        "project": "brain",
        "source": "live"
    });
    spec
}

pub async fn openapi_handler() -> impl axum::response::IntoResponse {
    axum::Json(brain_spec_json())
}

pub async fn openapi_public_handler() -> impl axum::response::IntoResponse {
    axum::Json(brain_scanner::filter_brain_public(brain_spec_json()))
}
