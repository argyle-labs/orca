use std::collections::HashMap;
use std::path::PathBuf;

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use utoipa::ToSchema;

use super::prelude::*;
use super::{DockerActionRequest, DockerActionResponse};

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
    let (engine, running) = detect_docker_engine().await;
    Json(json!({ "engine": engine, "running": running })).into_response()
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
    // Only know how to start Colima — Docker Desktop requires a UI interaction.
    let colima = Command::new("which").arg("colima").output().await;
    let has_colima = colima.map(|o| o.status.success()).unwrap_or(false);
    if !has_colima {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "colima not found — start Docker Desktop manually");
    }
    match Command::new("colima").arg("start").output().await {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let output = if stdout.is_empty() { stderr } else { stdout };
            Json(json!({ "output": output })).into_response()
        }
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
    let compose_file = match find_compose_file(&params.path) {
        Some(f) => f,
        None => return Json(json!({ "services": [], "composeFile": null })).into_response(),
    };

    let names_out = match run_docker(
        &[
            "compose",
            "-f",
            compose_file.to_str().unwrap(),
            "config",
            "--services",
        ],
        None,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let names: Vec<String> = names_out
        .trim()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let ps_raw = run_docker(
        &[
            "compose",
            "-f",
            compose_file.to_str().unwrap(),
            "ps",
            "--format",
            "json",
        ],
        None,
    )
    .await
    .unwrap_or_default();
    let statuses = parse_compose_ps(&ps_raw);

    let services: Vec<Value> = names
        .iter()
        .map(|name| {
            let s = statuses.get(name.as_str()).cloned().unwrap_or_default();
            json!({
                "name": name,
                "state": s.state,
                "running": s.state.to_lowercase().contains("running"),
                "health": s.health,
                "ports": s.ports,
            })
        })
        .collect();

    Json(json!({ "composeFile": compose_file, "services": services })).into_response()
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
    let compose_file = match find_compose_file(&body.project_path) {
        Some(f) => f,
        None => return err(StatusCode::NOT_FOUND, "no compose file found"),
    };
    let cf = compose_file.to_str().unwrap().to_string();
    let svc: Vec<String> = body
        .service
        .as_deref()
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    let tail_str;

    let args: Vec<String> = match body.action.as_str() {
        "start" => {
            let mut a = vec!["compose".into(), "-f".into(), cf, "start".into()];
            a.extend(svc);
            a
        }
        "stop" => {
            let mut a = vec!["compose".into(), "-f".into(), cf, "stop".into()];
            a.extend(svc);
            a
        }
        "restart" => {
            let mut a = vec!["compose".into(), "-f".into(), cf, "restart".into()];
            a.extend(svc);
            a
        }
        "up" => {
            let mut a = vec!["compose".into(), "-f".into(), cf, "up".into(), "-d".into()];
            a.extend(svc);
            a
        }
        "down" => {
            if let Some(ref s) = body.service {
                vec!["compose".into(), "-f".into(), cf, "stop".into(), s.clone()]
            } else {
                vec!["compose".into(), "-f".into(), cf, "down".into()]
            }
        }
        "build" => {
            let mut a = vec![
                "compose".into(),
                "-f".into(),
                cf,
                "build".into(),
                "--no-cache".into(),
            ];
            a.extend(svc);
            a
        }
        "pull" => {
            let mut a = vec!["compose".into(), "-f".into(), cf, "pull".into()];
            a.extend(svc);
            a
        }
        "logs" => {
            tail_str = body.tail.unwrap_or(100).to_string();
            let mut a = vec![
                "compose".into(),
                "-f".into(),
                cf,
                "logs".into(),
                "--tail".into(),
                tail_str.clone(),
                "--no-color".into(),
            ];
            a.extend(svc);
            a
        }
        "ps" => vec![
            "compose".into(),
            "-f".into(),
            cf,
            "ps".into(),
            "--format".into(),
            "json".into(),
        ],
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("unknown action: {}", body.action),
            );
        }
    };

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match run_docker(&args_ref, None).await {
        Ok(output) => Json(DockerActionResponse {
            output,
            compose_file: compose_file.to_str().map(|s| s.to_string()),
        })
        .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── Docker helpers ────────────────────────────────────────────────────────────

pub(crate) fn find_compose_file(project_path: &str) -> Option<PathBuf> {
    for name in &[
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ] {
        let full = PathBuf::from(project_path).join(name);
        if full.exists() {
            return Some(full);
        }
    }
    None
}

/// Returns `("colima" | "desktop" | "none", is_running)`.
async fn detect_docker_engine() -> (&'static str, bool) {
    // Check Colima first — if installed and running it takes priority.
    let colima_ok = Command::new("which").arg("colima").output().await
        .map(|o| o.status.success()).unwrap_or(false);
    if colima_ok {
        let status = Command::new("colima").arg("status").output().await;
        if let Ok(out) = status {
            let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
            let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
            let combined = format!("{text}{stderr}");
            if combined.contains("running") {
                return ("colima", true);
            }
        }
        return ("colima", false);
    }

    // Fall back: probe Docker Desktop by pinging the daemon.
    let ping = Command::new("docker").args(["info", "--format", "{{.ServerVersion}}"]).output().await;
    let running = ping.map(|o| o.status.success()).unwrap_or(false);
    ("desktop", running)
}

/// Returns the DOCKER_HOST value to inject, or None if default socket is fine.
async fn docker_host() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    // Colima's default socket
    let colima_sock = format!("{home}/.colima/default/docker.sock");
    if std::path::Path::new(&colima_sock).exists() {
        return Some(format!("unix://{colima_sock}"));
    }
    None
}

pub(crate) async fn run_docker(args: &[&str], cwd: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = Command::new("docker");
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(host) = docker_host().await {
        cmd.env("DOCKER_HOST", host);
    }
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() && stdout.trim().is_empty() {
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

#[derive(Default, Clone)]
pub(crate) struct ServiceStatus {
    pub state: String,
    pub health: String,
    pub ports: Vec<String>,
}

pub(crate) fn parse_compose_ps(raw: &str) -> HashMap<String, ServiceStatus> {
    let mut out = HashMap::new();
    for line in raw.trim().lines() {
        let Ok(obj): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        let name = obj["Service"]
            .as_str()
            .or_else(|| obj["service"].as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let ports = obj["Publishers"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|p| {
                let pub_port = p["PublishedPort"].as_u64()?;
                let target = p["TargetPort"].as_u64()?;
                if pub_port == 0 {
                    return None;
                }
                Some(format!("{pub_port}:{target}"))
            })
            .collect();
        out.insert(
            name,
            ServiceStatus {
                state: obj["State"].as_str().unwrap_or("unknown").to_string(),
                health: obj["Health"].as_str().unwrap_or("").to_string(),
                ports,
            },
        );
    }
    out
}
