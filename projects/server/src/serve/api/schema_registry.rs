use axum::{http::StatusCode, response::{IntoResponse, Json, Response}};

use super::prelude::*;

// ── GET /api/schema/databases ─────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema/databases",
    operation_id = "listSchemaDatabases",
    responses(
        (status = 200, description = "All registered schema databases", body = Vec<SchemaDbInfo>),
        (status = 500, body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_databases_handler() -> Response {
    match brain_utils::db::open_default() {
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(conn) => match brain_utils::db::list_schema_databases(&conn) {
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            Ok(rows) => {
                let dbs: Vec<SchemaDbInfo> = rows
                    .into_iter()
                    .map(|r| SchemaDbInfo {
                        name: r.name,
                        host: r.host,
                        port: r.port,
                        user: r.user,
                        database: r.database,
                        container: r.container,
                        domains_file: r.domains_file,
                        enabled: r.enabled,
                    })
                    .collect();
                Json(dbs).into_response()
            }
        },
    }
}

// ── POST /api/schema/databases ────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/schema/databases",
    operation_id = "addSchemaDatabase",
    request_body = SchemaDbAddRequest,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_databases_add_handler(Json(body): Json<SchemaDbAddRequest>) -> Response {
    let row = brain_utils::db::SchemaDbRow {
        name: body.name,
        host: body.host,
        port: body.port,
        user: body.user,
        password: body.password,
        database: body.database,
        container: body.container,
        domains_file: body.domains_file,
        enabled: true,
    };
    match brain_utils::db::open_default()
        .and_then(|conn| brain_utils::db::upsert_schema_database(&conn, &row))
    {
        Ok(()) => Json(OkResponse { ok: true }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── DELETE /api/schema/databases/:name ────────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/schema/databases/{name}",
    operation_id = "removeSchemaDatabase",
    params(("name" = String, Path, description = "Database name")),
    responses(
        (status = 200, body = OkResponse),
        (status = 404, body = ErrorResponse),
        (status = 500, body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_databases_remove_handler(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Response {
    match brain_utils::db::open_default()
        .and_then(|conn| brain_utils::db::remove_schema_database(&conn, &name))
    {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, &format!("database '{name}' not found")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
