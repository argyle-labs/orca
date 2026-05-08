use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use super::prelude::*;
use super::{HealthCheck, HealthResponse};
use crate::serve::middleware::CorrelationId;

// ── GET /api/health ───────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/health",
    operation_id = "ping",
    responses(
        (status = 200, description = "Server is alive"),
    ),
    tag = "health"
)]
pub async fn ping_handler() -> impl IntoResponse {
    Json(json!({ "ok": true }))
}

// ── GET /api/rebuy/health/local ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/rebuy/health/local",
    operation_id = "getHealth",
    responses(
        (status = 200, description = "All service health checks", body = HealthResponse),
        (status = 503, description = "rebuy MCP unavailable", body = ErrorResponse),
    ),
    tag = "health"
)]
pub async fn rebuy_health_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
    const CHECKS: &[(&str, &str)] = &[
        ("DB", "rebuy_db_status"),
        ("Env", "rebuy_env_status"),
        ("Engines", "rebuy_engines_status"),
        ("Tunnel", "rebuy_tunnel_status"),
        ("Network", "rebuy_network_status"),
        ("Mode", "rebuy_mode_current"),
    ];

    let client = match pool.get_or_connect("rebuy").await {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("rebuy MCP unavailable: {e}"),
            );
        }
    };

    let futures: Vec<_> = CHECKS
        .iter()
        .map(|(label, tool)| {
            let client = client.clone();
            let label = label.to_string();
            let tool = tool.to_string();
            let cid = cid.clone();
            async move {
                let result = client.call_tool(&tool, json!({}), &cid).await;
                let output = match &result {
                    Ok(v) => v["content"]
                        .get(0)
                        .and_then(|c| c["text"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    Err(e) => format!("error: {e}"),
                };
                let ok = result.is_ok() && !output.to_lowercase().contains("error");
                HealthCheck {
                    label,
                    tool,
                    output,
                    ok,
                }
            }
        })
        .collect();

    let checks = futures_util::future::join_all(futures).await;
    Json(HealthResponse {
        timestamp: chrono::Utc::now().to_rfc3339(),
        checks,
    })
    .into_response()
}
