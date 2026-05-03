use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use super::prelude::*;
use super::{McpRunRequest, McpRunResponse, McpToolInfo};
use crate::serve::middleware::CorrelationId;

// ── GET /api/mcp/servers ──────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/mcp/servers",
    operation_id = "listMcpServers",
    responses(
        (status = 200, description = "All registered MCP servers", body = Vec<McpServerInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_servers_handler() -> Response {
    db_json(|| {
        let conn = orca_utils::db::open_default()?;
        let servers: Vec<McpServerInfo> = orca_utils::db::list_mcp_servers(&conn)?
            .into_iter()
            .map(|r| McpServerInfo {
                name: r.name,
                command: r.command,
                args: r.args,
                env: r.env,
                enabled: r.enabled,
            })
            .collect();
        Ok(servers)
    })
}

// ── POST /api/mcp/servers ─────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/mcp/servers",
    operation_id = "addMcpServer",
    request_body = McpServerAddRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_add_handler(Json(body): Json<McpServerAddRequest>) -> Response {
    db_ok(|| {
        let row = orca_utils::db::McpServerRow {
            name: body.name,
            command: body.command,
            args: body.args,
            env: body.env,
            enabled: true,
        };
        let conn = orca_utils::db::open_default()?;
        orca_utils::db::upsert_mcp_server(&conn, &row)
    })
}

// ── DELETE /api/mcp/servers/:name ─────────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/mcp/servers/{name}",
    operation_id = "removeMcpServer",
    params(("name" = String, Path, description = "Server name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_remove_handler(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    db_remove("server", &name, || {
        let conn = orca_utils::db::open_default()?;
        orca_utils::db::remove_mcp_server(&conn, &name)
    })
}

// ── GET /api/mcp/tools ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/mcp/tools",
    operation_id = "getMcpTools",
    responses(
        (status = 200, description = "All tools from all connected MCP servers", body = Vec<McpToolInfo>),
    ),
    tag = "mcp"
)]
pub async fn mcp_tools_handler(State(pool): State<McpState>) -> impl IntoResponse {
    let tools = pool.all_tools().await;
    Json(tools)
}

// ── POST /api/mcp/run ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/mcp/run",
    operation_id = "runMcpTool",
    request_body = McpRunRequest,
    responses(
        (status = 200, description = "Tool result", body = McpRunResponse),
        (status = 404, description = "Unknown MCP server", body = ErrorResponse),
        (status = 500, description = "Tool execution error", body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_run_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
    Json(body): Json<McpRunRequest>,
) -> Response {
    match pool.get_or_connect(&body.server).await {
        Err(e) => err(StatusCode::NOT_FOUND, &e.to_string()),
        Ok(client) => {
            let args = body.arguments.unwrap_or(json!({}));
            match client.call_tool(&body.name, args, &cid).await {
                Ok(result) => Json(result).into_response(),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("MCP server closed") {
                        pool.evict(&body.server).await;
                    }
                    err(StatusCode::INTERNAL_SERVER_ERROR, &msg)
                }
            }
        }
    }
}
