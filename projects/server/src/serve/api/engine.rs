//! Engine REST surface — typed axum handlers that delegate to the OrcaTool
//! impls in `crate::mcp::engine_tools`.
//!
//! The tool impls are the single source of truth for behaviour; these
//! handlers are thin shims providing the typed REST + OpenAPI surface. The
//! follow-up `#[orca_op]` proc-macro will generate this file from the tool
//! impl directly.

use super::prelude::*;
use crate::mcp::engine_tools::{
    AddArgs, EmptyArgs, EngineAdd, EngineDisable, EngineEnable, EngineList, EngineRemove, NameArgs,
};
use axum::response::IntoResponse;
use orca_utils::config::Config;
use orca_utils::tool::{OrcaTool, ToolCtx};
use std::sync::Arc;

fn ctx() -> Result<ToolCtx, String> {
    Config::load()
        .map(|cfg| ToolCtx::new(Arc::new(cfg)))
        .map_err(|e| e.to_string())
}

#[allow(clippy::result_large_err)] // Response is large but this is a one-shot early-exit
fn ctx_or_500(r: Result<ToolCtx, String>) -> Result<ToolCtx, axum::response::Response> {
    r.map_err(|e| err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e))
}

// ── GET /api/engines ─────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/engines",
    operation_id = "listEngines",
    responses(
        (status = 200, description = "Registered LLM backends", body = Vec<LlmProviderInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "engines"
)]
pub async fn engines_list_handler() -> axum::response::Response {
    let ctx = match ctx_or_500(ctx()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match EngineList::run(EmptyArgs {}, &ctx).await {
        Ok(json) => match serde_json::from_str::<Vec<LlmProviderInfo>>(&json) {
            Ok(v) => axum::Json(v).into_response(),
            Err(e) => err(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
            ),
        },
        Err(e) => err(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
        ),
    }
}

// ── POST /api/engines ────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/engines",
    operation_id = "addEngine",
    request_body = LlmProviderAddRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 400, body = ErrorResponse),
    ),
    tag = "engines"
)]
pub async fn engines_add_handler(
    axum::Json(body): axum::Json<LlmProviderAddRequest>,
) -> axum::response::Response {
    let ctx = match ctx_or_500(ctx()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let args = AddArgs {
        name: body.name,
        url: body.url,
        kind: body.kind.unwrap_or_default(),
    };
    match EngineAdd::run(args, &ctx).await {
        Ok(_) => axum::Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

// ── DELETE /api/engines/{name} ───────────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/engines/{name}",
    operation_id = "removeEngine",
    params(("name" = String, Path, description = "Engine name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
    ),
    tag = "engines"
)]
pub async fn engines_remove_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    let ctx = match ctx_or_500(ctx()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match EngineRemove::run(NameArgs { name }, &ctx).await {
        Ok(_) => axum::Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(axum::http::StatusCode::NOT_FOUND, &e.to_string()),
    }
}

// ── PATCH /api/engines/{name}/enable ─────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/api/engines/{name}/enable",
    operation_id = "enableEngine",
    params(("name" = String, Path, description = "Engine name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
    ),
    tag = "engines"
)]
pub async fn engines_enable_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    let ctx = match ctx_or_500(ctx()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match EngineEnable::run(NameArgs { name }, &ctx).await {
        Ok(_) => axum::Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(axum::http::StatusCode::NOT_FOUND, &e.to_string()),
    }
}

// ── PATCH /api/engines/{name}/disable ────────────────────────────────────────

#[utoipa::path(
    patch,
    path = "/api/engines/{name}/disable",
    operation_id = "disableEngine",
    params(("name" = String, Path, description = "Engine name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
    ),
    tag = "engines"
)]
pub async fn engines_disable_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> axum::response::Response {
    let ctx = match ctx_or_500(ctx()) {
        Ok(c) => c,
        Err(r) => return r,
    };
    match EngineDisable::run(NameArgs { name }, &ctx).await {
        Ok(_) => axum::Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(axum::http::StatusCode::NOT_FOUND, &e.to_string()),
    }
}
