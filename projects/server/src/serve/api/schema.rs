//! HTTP handlers for `/api/schema` + `/api/schema/domains`. All
//! introspection logic lives in the `schema` crate; this module only adapts
//! the typed result to the axum/utoipa surface.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use schema::view::{SchemaBuildError, build_schema_domains, build_schema_response};

use super::prelude::*;

// ── GET /api/schema ───────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema",
    operation_id = "getSchema",
    responses(
        (status = 200, description = "Database schema tabs", body = super::SchemaResponse),
        (status = 404, description = "No databases configured", body = ErrorResponse),
        (status = 500, description = "All DB connections failed", body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_handler() -> Response {
    match build_schema_response().await {
        Ok(v) => Json(v).into_response(),
        Err(SchemaBuildError::NoDatabases) => err(
            StatusCode::NOT_FOUND,
            &SchemaBuildError::NoDatabases.to_string(),
        ),
        Err(e @ SchemaBuildError::AllFailed(_)) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

// ── GET /api/schema/domains ───────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema/domains",
    operation_id = "getSchemaDomains",
    responses(
        (status = 200, description = "All domain definitions from all configured databases"),
    ),
    tag = "schema"
)]
pub async fn schema_domains_handler() -> Response {
    Json(build_schema_domains()).into_response()
}
