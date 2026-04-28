use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use super::{McpRunRequest, McpState, err};
use crate::serve::middleware::CorrelationId;

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
        (status = 404, description = "Unknown MCP server", body = super::ErrorResponse),
        (status = 500, description = "Tool execution error", body = super::ErrorResponse),
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
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
    }
}
