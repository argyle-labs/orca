#![allow(clippy::disallowed_types)] // OpenAPI spec construction — dynamic JSON required
use std::sync::OnceLock;

use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth_routes;
use ::mcp::client::McpPool;

/// Static OpenAPI doc skeleton — info + tags. Paths are injected at
/// `orca_spec_json()` time from every `#[orca_tool]` registration.
/// Auth routes are still hand-written (sessions/cookies) so they
/// remain on the static router via `routes!()`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "orca API",
        version = "0.1.0",
        description = "orca — typed tool dispatch via #[orca_tool]; auth is the only hand-written REST surface"
    ),
    components(schemas(
        auth_routes::SignupRequest,
        auth_routes::SigninRequest,
        auth_routes::ChangePasswordRequest,
        auth_routes::ChangePasswordOk,
        auth_routes::SessionOk,
        auth_routes::SignupStatus,
        auth_routes::MeOk,
        auth_routes::AuthErrorResponse,
    )),
    tags(
        (name = "auth", description = "Browser sign-up / sign-in / session management"),
    )
)]
pub struct ApiDoc;

static SPEC: OnceLock<utoipa::openapi::OpenApi> = OnceLock::new();

pub(super) fn openapi_router() -> OpenApiRouter<std::sync::Arc<McpPool>> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(auth_routes::signup_status))
        .routes(routes!(auth_routes::signup))
        .routes(routes!(auth_routes::signin))
        .routes(routes!(auth_routes::signout))
        .routes(routes!(auth_routes::change_password))
        .routes(routes!(auth_routes::me))
}

pub(super) fn install_spec(mut spec: utoipa::openapi::OpenApi) {
    spec.info.version = env!("CARGO_PKG_VERSION").to_string();
    _ = SPEC.set(spec);
}

fn build_spec() -> utoipa::openapi::OpenApi {
    let (_, mut spec) = openapi_router().split_for_parts();
    spec.info.version = env!("CARGO_PKG_VERSION").to_string();
    spec
}

pub fn orca_spec_json() -> serde_json::Value {
    let spec = SPEC.get().cloned().unwrap_or_else(build_spec);
    let mut value = serde_json::to_value(&spec).unwrap_or_default();
    dispatch::openapi::inject_tool_paths(&mut value);
    // Live, plugin-driven unit surface — reflects currently-loaded providers.
    dispatch::openapi::inject_unit_paths(&mut value);
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
    axum::Json(db::openapi_specs_registry::filter_orca_public(
        orca_spec_json(),
    ))
}

/// Live managed-unit catalog — every `unit.<kind>.<verb|action>` op the loaded
/// providers currently expose, with typed input/output schemas. The `orca unit`
/// CLI fetches this to build its command tree + `--help` against what's actually
/// running, giving runtime service discovery with type hints.
pub async fn unit_catalog_handler() -> impl axum::response::IntoResponse {
    axum::Json(dispatch::unit_surface::unit_catalog_json())
}

// CLI / MCP sister spec handlers were deleted 2026-06-07: the unified
// spec now emits per-operation `x-codeSamples` (REST/CLI/MCP tabs) and
// `x-tagGroups` (hierarchical nav) directly from `dispatch::openapi`, so
// one Scalar page renders all three surfaces. See
// `projects/dispatch/src/openapi.rs::inject_tool_paths`.
