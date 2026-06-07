#![allow(clippy::disallowed_types)] // OpenAPI spec construction — dynamic JSON required
use serde_json as sj;
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

pub async fn openapi_cli_handler() -> impl axum::response::IntoResponse {
    axum::Json(reframe_spec(orca_spec_json(), Surface::Cli))
}

pub async fn openapi_mcp_handler() -> impl axum::response::IntoResponse {
    axum::Json(reframe_spec(orca_spec_json(), Surface::Mcp))
}

#[derive(Clone, Copy)]
enum Surface {
    Cli,
    Mcp,
}

/// Reframe the live OpenAPI spec for a non-REST surface (CLI or MCP).
/// Operates on the already-dynamic spec value returned by
/// `orca_spec_json()` — the workspace-wide ban on opaque JSON is
/// opted out at the top of this file because OpenAPI construction
/// is dynamic by nature.
///
/// Every `/api/v1/<domain>.<verb>` operation keeps its request /
/// response schemas (the tool contract is identical across surfaces),
/// but `summary` is rewritten to the surface-native invocation form
/// (`orca <d> <v>` or `<d>_<v>`) and `description` is prefixed with
/// that form so it lands prominently in the Scalar overview panel.
fn reframe_spec(mut spec: sj::Value, surface: Surface) -> sj::Value {
    use sj::Value as J;
    let (title, blurb) = match surface {
        Surface::Cli => (
            "orca CLI surface",
            "Same tool registry as REST and MCP, framed as `orca <domain> <verb>` invocations.",
        ),
        Surface::Mcp => (
            "orca MCP surface",
            "Same tool registry as REST and CLI, framed as MCP `tools/list` entries (call via JSON-RPC `tools/call`).",
        ),
    };
    if let Some(info) = spec.get_mut("info").and_then(|v| v.as_object_mut()) {
        info.insert("title".into(), J::String(title.into()));
        info.insert("description".into(), J::String(blurb.into()));
    }
    let Some(paths) = spec.get_mut("paths").and_then(|v| v.as_object_mut()) else {
        return spec;
    };
    for (path, ops) in paths.iter_mut() {
        let Some(rest) = path.strip_prefix("/api/v1/") else {
            continue;
        };
        if !rest.contains('.') {
            continue;
        }
        let invocation = match surface {
            Surface::Cli => format!("orca {}", rest.replace('.', " ")),
            Surface::Mcp => rest.replace('.', "_"),
        };
        let Some(ops_obj) = ops.as_object_mut() else {
            continue;
        };
        for op in ops_obj.values_mut() {
            let Some(op_obj) = op.as_object_mut() else {
                continue;
            };
            let original_summary = op_obj
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let original_desc = op_obj
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            op_obj.insert("summary".into(), J::String(invocation.clone()));
            let mut new_desc = format!("**`{invocation}`**\n\n");
            if !original_summary.is_empty() {
                new_desc.push_str(&original_summary);
                new_desc.push_str("\n\n");
            }
            if !original_desc.is_empty() {
                new_desc.push_str(&original_desc);
            }
            op_obj.insert("description".into(), J::String(new_desc));
        }
    }
    spec
}
