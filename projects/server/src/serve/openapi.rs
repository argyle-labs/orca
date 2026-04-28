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
        // registry (internal — brain-local only)
        super::api::specs_list_handler,
        super::api::specs_get_handler,
        super::api::specs_get_public_handler,
    ),
    components(schemas(
        super::api::TreeNode,
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
        super::api::SpecFiles,
        super::api::SpecMeta,
    )),
    tags(
        // Public domains — served at /api/openapi/public.json
        (name = "docs",    description = "Brain vault document tree and search [public]"),
        (name = "library", description = "Library documentation via context7 [public]"),
        // Internal domains — brain local use only
        (name = "mcp",     description = "MCP tool proxy — run any connected MCP server tool"),
        (name = "docker",  description = "Docker Compose service management"),
        (name = "schema",  description = "MySQL schema visualizer"),
        (name = "health",  description = "Rebuy service health checks"),
        (name = "logs",    description = "Docker service log streaming"),
        (name = "specs",   description = "External API spec registry"),
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
