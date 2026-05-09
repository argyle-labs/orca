use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use orca_docker::{Compose, ComposeError};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use utoipa::ToSchema;

use super::prelude::*;
use super::{DockerActionRequest, DockerActionResponse};

// Re-exported helper used by tests_handler.rs.
pub(crate) use orca_docker::compose::parse_compose_ps;

/// Compatibility shim — keeps the old call sites working unchanged.
pub(crate) fn find_compose_file(project_path: &str) -> Option<std::path::PathBuf> {
    Compose::find(Path::new(project_path)).map(|c| c.file().to_path_buf())
}

/// Compatibility shim around the now-public `orca_docker::run`.
pub(crate) async fn run_docker(args: &[&str], cwd: Option<&str>) -> anyhow::Result<String> {
    orca_docker::run(args, cwd).await
}

// ── GET /api/docker/engine ────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/docker/engine",
    operation_id = "getDockerEngine",
    responses(
        (status = 200, description = "Docker engine status"),
    ),
    tag = "docker"
)]
pub async fn docker_engine_handler() -> Response {
    let status = orca_docker::engine::status().await;
    let label = match status.engine {
        orca_docker::Engine::Colima => "colima",
        orca_docker::Engine::Desktop => "desktop",
        orca_docker::Engine::None => "none",
    };
    Json(json!({ "engine": label, "running": status.running })).into_response()
}

// ── POST /api/docker/engine/start ─────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/docker/engine/start",
    operation_id = "startDockerEngine",
    responses(
        (status = 200, description = "Engine start output"),
        (status = 500, description = "Failed to start engine", body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_engine_start_handler() -> Response {
    match orca_docker::engine::start().await {
        Ok(output) => Json(json!({ "output": output })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── GET /api/docker/services ──────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct DockerServicesQuery {
    pub path: String,
}

#[utoipa::path(
    get,
    path = "/api/docker/services",
    operation_id = "getDockerServices",
    params(
        ("path" = String, Query, description = "Absolute path to the Docker Compose project directory"),
    ),
    responses(
        (status = 200, description = "Compose file path and service list", body = super::DockerServicesResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_services_handler(Query(params): Query<DockerServicesQuery>) -> Response {
    let compose = match Compose::find(Path::new(&params.path)) {
        Some(c) => c,
        None => return Json(json!({ "services": [], "composeFile": null })).into_response(),
    };
    match compose.services().await {
        Ok(services) => {
            let services_json: Vec<Value> = services
                .into_iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "state": s.state,
                        "running": s.running,
                        "health": s.health,
                        "ports": s.ports,
                    })
                })
                .collect();
            Json(json!({
                "composeFile": compose.file(),
                "services": services_json,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── POST /api/docker/action ───────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/docker/action",
    operation_id = "runDockerAction",
    request_body = DockerActionRequest,
    responses(
        (status = 200, description = "Action output", body = DockerActionResponse),
        (status = 400, description = "Unknown action", body = ErrorResponse),
        (status = 404, description = "No compose file", body = ErrorResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_action_handler(Json(body): Json<DockerActionRequest>) -> Response {
    let compose = match Compose::find(Path::new(&body.project_path)) {
        Some(c) => c,
        None => return err(StatusCode::NOT_FOUND, "no compose file found"),
    };
    match compose
        .run_action(body.action.as_str(), body.service.as_deref(), body.tail)
        .await
    {
        Ok(output) => Json(DockerActionResponse {
            output,
            compose_file: compose.file().to_str().map(|s| s.to_string()),
        })
        .into_response(),
        Err(ComposeError::UnknownAction(a)) => {
            err(StatusCode::BAD_REQUEST, &format!("unknown action: {a}"))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

