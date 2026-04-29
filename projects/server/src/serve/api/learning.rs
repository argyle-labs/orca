use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use brain_utils::db;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use super::prelude::*;

#[derive(Serialize, ToSchema)]
pub struct ProgressResponse {
    pub page: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct ProgressRequest {
    pub page: String,
}

// ── GET /api/learning/progress ────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/learning/progress",
    operation_id = "getLearningProgress",
    responses(
        (status = 200, description = "Last visited learning page", body = ProgressResponse),
    ),
    tag = "learning"
)]
pub async fn get_progress_handler() -> Response {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let page = match db::get_learning_progress(&conn) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    Json(ProgressResponse { page }).into_response()
}

// ── POST /api/learning/progress ───────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/learning/progress",
    operation_id = "saveLearningProgress",
    request_body = ProgressRequest,
    responses(
        (status = 200, description = "Progress saved"),
    ),
    tag = "learning"
)]
pub async fn save_progress_handler(Json(body): Json<ProgressRequest>) -> Response {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    match db::save_learning_progress(&conn, &body.page) {
        Ok(_) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
