use axum::{http::StatusCode, response::{IntoResponse, Json, Response}};

use super::prelude::*;

// ── GET /api/docker/runtimes ──────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/docker/runtimes",
    operation_id = "listDockerRuntimes",
    responses(
        (status = 200, description = "All registered Docker runtimes", body = Vec<DockerRuntimeInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_runtimes_handler() -> Response {
    match brain_utils::db::open_default() {
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(conn) => match brain_utils::db::list_docker_runtimes(&conn) {
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            Ok(rows) => {
                let rts: Vec<DockerRuntimeInfo> = rows
                    .into_iter()
                    .map(|r| DockerRuntimeInfo {
                        name: r.name,
                        socket_path: r.socket_path,
                        host: r.host,
                        url: r.url,
                        enabled: r.enabled,
                    })
                    .collect();
                Json(rts).into_response()
            }
        },
    }
}

// ── POST /api/docker/runtimes ─────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/docker/runtimes",
    operation_id = "addDockerRuntime",
    request_body = DockerRuntimeAddRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_runtimes_add_handler(Json(body): Json<DockerRuntimeAddRequest>) -> Response {
    let row = brain_utils::db::DockerRuntimeRow {
        name: body.name,
        socket_path: body.socket_path,
        host: body.host,
        url: body.url,
        enabled: true,
    };
    match brain_utils::db::open_default()
        .and_then(|conn| brain_utils::db::upsert_docker_runtime(&conn, &row))
    {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── DELETE /api/docker/runtimes/:name ─────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/docker/runtimes/{name}",
    operation_id = "removeDockerRuntime",
    params(("name" = String, Path, description = "Runtime name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_runtimes_remove_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    match brain_utils::db::open_default()
        .and_then(|conn| brain_utils::db::remove_docker_runtime(&conn, &name))
    {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, &format!("runtime '{name}' not found")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
