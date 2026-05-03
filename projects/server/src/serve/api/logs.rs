use std::path::PathBuf;

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use super::prelude::*;
use super::LogsResponse;
use super::docker::{find_compose_file, parse_compose_ps, run_docker};

// ── GET /api/logs/services ────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/logs/services",
    operation_id = "getLogServices",
    responses(
        (status = 200, description = "All Docker projects and their service states", body = super::LogServicesResponse),
        (status = 404, description = "Rebuy root not found", body = ErrorResponse),
    ),
    tag = "logs"
)]
pub async fn log_services_handler() -> Response {
    let home = std::env::var("HOME").unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"));

    let project_dirs: Vec<PathBuf> = match std::fs::read_dir(&rebuy_root) {
        Err(_) => return err(StatusCode::NOT_FOUND, "rebuy root not found"),
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.is_dir() && find_compose_file(p.to_str()?).is_some() {
                    Some(p)
                } else {
                    None
                }
            })
            .collect(),
    };

    let futures: Vec<_> = project_dirs
        .iter()
        .map(|project_path| {
            let project_path = project_path.clone();
            async move {
                let path_str = project_path.to_string_lossy().to_string();
                let name = project_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_str.clone());
                let compose_file = match find_compose_file(&path_str) {
                    Some(f) => f,
                    None => return json!({ "project": name, "path": path_str, "services": [] }),
                };
                let compose_str = compose_file.to_string_lossy().into_owned();
                let ps_raw = run_docker(
                    &[
                        "compose",
                        "-f",
                        &compose_str,
                        "ps",
                        "--format",
                        "json",
                    ],
                    None,
                )
                .await
                .unwrap_or_default();
                let statuses = parse_compose_ps(&ps_raw);
                let services: Vec<serde_json::Value> = statuses
                    .iter()
                    .map(|(svc_name, s)| {
                        json!({
                            "name": svc_name, "state": s.state,
                            "running": s.state.to_lowercase().contains("running"),
                            "health": s.health, "ports": s.ports,
                        })
                    })
                    .collect();
                json!({ "project": name, "path": path_str, "services": services })
            }
        })
        .collect();

    let projects = futures_util::future::join_all(futures).await;
    Json(json!({ "projects": projects })).into_response()
}

// ── GET /api/logs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct LogsQuery {
    pub project: String,
    pub service: Option<String>,
    pub tail: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/logs",
    operation_id = "getLogs",
    params(
        ("project" = String, Query, description = "Absolute path to the project directory"),
        ("service" = Option<String>, Query, description = "Specific service name (omit for all)"),
        ("tail" = Option<u32>, Query, description = "Number of log lines to return (default 200)"),
    ),
    responses(
        (status = 200, description = "Log output", body = LogsResponse),
        (status = 404, description = "No compose file found", body = ErrorResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "logs"
)]
pub async fn log_fetch_handler(Query(params): Query<LogsQuery>) -> Response {
    let compose_file = match find_compose_file(&params.project) {
        Some(f) => f,
        None => return err(StatusCode::NOT_FOUND, "no compose file found"),
    };
    let cf = compose_file.to_string_lossy().into_owned();
    let tail_str = params.tail.unwrap_or(200).to_string();
    let mut args = vec![
        "compose",
        "-f",
        &cf,
        "logs",
        "--tail",
        &tail_str,
        "--no-color",
    ];
    let svc_owned = params.service.clone();
    if let Some(ref s) = svc_owned {
        args.push(s.as_str());
    }
    match run_docker(&args, None).await {
        Ok(output) => Json(LogsResponse { output }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
