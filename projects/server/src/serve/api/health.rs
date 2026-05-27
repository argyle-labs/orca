use axum::response::{IntoResponse, Json};
use serde_json::json;

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
