use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::prelude::*;

#[derive(Serialize, ToSchema)]
pub struct MappingRow {
    pub orca_tool: String,
    pub mcp_name: String,
    pub external_tool: String,
    pub match_type: String,
    pub confidence: Option<f64>,
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct MappingsQuery {
    pub name: Option<String>,
}

// ── GET /api/mcp/mappings ─────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/mcp/mappings",
    operation_id = "listMcpMappings",
    params(("name" = Option<String>, Query, description = "Filter by MCP server name")),
    responses(
        (status = 200, body = Vec<MappingRow>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_mappings_list_handler(Query(q): Query<MappingsQuery>) -> Response {
    match db::open_default() {
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(conn) => {
            let result = if let Some(name) = &q.name {
                db::list_mcp_tool_mappings(&conn, name)
            } else {
                db::all_mcp_tool_mappings(&conn)
            };
            match result {
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
                Ok(rows) => {
                    let mapped: Vec<MappingRow> = rows
                        .into_iter()
                        .map(|r| MappingRow {
                            orca_tool: r.orca_tool,
                            mcp_name: r.mcp_name,
                            external_tool: r.external_tool,
                            match_type: r.match_type,
                            confidence: r.confidence,
                            enabled: r.enabled,
                        })
                        .collect();
                    Json(mapped).into_response()
                }
            }
        }
    }
}

// ── POST /api/mcp/mappings ────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct MapRequest {
    pub name: String,
    pub orca_tool: String,
    pub external_tool: String,
}

#[utoipa::path(
    post,
    path = "/api/mcp/mappings",
    operation_id = "createMcpMapping",
    request_body = MapRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_mappings_create_handler(Json(body): Json<MapRequest>) -> Response {
    let row = db::McpToolMappingRow {
        orca_tool: body.orca_tool.clone(),
        mcp_name: body.name,
        external_tool: body.external_tool,
        match_type: "explicit".to_string(),
        confidence: None,
        enabled: true,
    };
    match db::open_default().and_then(|conn| db::upsert_mcp_tool_mapping(&conn, &row)) {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── DELETE /api/mcp/mappings/:orca_tool ──────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/mcp/mappings/{orca_tool}",
    operation_id = "deleteMcpMapping",
    params(("orca_tool" = String, Path, description = "Orca tool name to unmap")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_mappings_delete_handler(
    axum::extract::Path(orca_tool): axum::extract::Path<String>,
) -> Response {
    match db::open_default().and_then(|conn| db::remove_mcp_tool_mapping(&conn, &orca_tool)) {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => err(
            StatusCode::NOT_FOUND,
            &format!("mapping '{orca_tool}' not found"),
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
