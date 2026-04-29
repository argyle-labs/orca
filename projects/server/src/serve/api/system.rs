use axum::{Json, response::{IntoResponse, Response}};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::prelude::*;

#[derive(Serialize, ToSchema)]
pub struct SystemStatusResponse {
    pub binary: ComponentStatus,
    pub claude_md: ComponentStatus,
    pub vault: ComponentStatus,
    pub agents: ComponentStatus,
    pub mcp: MpcStatus,
}

#[derive(Serialize, ToSchema)]
pub struct ComponentStatus {
    pub installed: bool,
    pub path: String,
}

#[derive(Serialize, ToSchema)]
pub struct MpcStatus {
    pub registered: bool,
}

#[derive(Serialize, ToSchema)]
pub struct SystemActionResponse {
    pub ok: bool,
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SystemActionRequest {
    /// "install" or "uninstall"
    pub action: String,
}

/// GET /api/system/status — installation status for the web UI
#[utoipa::path(
    get,
    path = "/api/system/status",
    responses(
        (status = 200, description = "Installation status", body = inline(serde_json::Value)),
        (status = 500, description = "Error", body = ErrorResponse),
    ),
    tag = "system"
)]
pub async fn system_status_handler() -> Response {
    let status = brain_commands::install_status();
    Json(status).into_response()
}

/// POST /api/system/action — run install or uninstall
#[utoipa::path(
    post,
    path = "/api/system/action",
    request_body = SystemActionRequest,
    responses(
        (status = 200, description = "Action result", body = SystemActionResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 500, description = "Error", body = ErrorResponse),
    ),
    tag = "system"
)]
pub async fn system_action_handler(
    Json(req): Json<SystemActionRequest>,
) -> Response {
    use brain_commands::install::{InstallReport, cmd_install_report, cmd_uninstall_report};

    let report: InstallReport = match req.action.as_str() {
        "install" => cmd_install_report(),
        "uninstall" => cmd_uninstall_report(),
        other => {
            return err(
                axum::http::StatusCode::BAD_REQUEST,
                &format!("unknown action '{other}' — use 'install' or 'uninstall'"),
            )
        }
    };

    Json(SystemActionResponse {
        ok: report.success(),
        done: report.done,
        skipped: report.skipped,
        errors: report.errors,
    })
    .into_response()
}
