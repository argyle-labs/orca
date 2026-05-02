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
pub async fn docker_runtimes_handler() -> axum::response::Response {
    db_json(|| {
        let conn = brain_utils::db::open_default()?;
        let rts: Vec<DockerRuntimeInfo> = brain_utils::db::list_docker_runtimes(&conn)?
            .into_iter()
            .map(|r| DockerRuntimeInfo {
                name: r.name,
                socket_path: r.socket_path,
                host: r.host,
                url: r.url,
                enabled: r.enabled,
            })
            .collect();
        Ok(rts)
    })
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
pub async fn docker_runtimes_add_handler(
    axum::Json(body): axum::Json<DockerRuntimeAddRequest>,
) -> axum::response::Response {
    db_ok(|| {
        let row = brain_utils::db::DockerRuntimeRow {
            name: body.name,
            socket_path: body.socket_path,
            host: body.host,
            url: body.url,
            enabled: true,
        };
        let conn = brain_utils::db::open_default()?;
        brain_utils::db::upsert_docker_runtime(&conn, &row)
    })
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
) -> axum::response::Response {
    db_remove("runtime", &name, || {
        let conn = brain_utils::db::open_default()?;
        brain_utils::db::remove_docker_runtime(&conn, &name)
    })
}
