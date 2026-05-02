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
pub async fn schema_databases_handler() -> axum::response::Response {
    db_json(|| {
        let conn = brain_utils::db::open_default()?;
        let dbs: Vec<SchemaDbInfo> = brain_utils::db::list_schema_databases(&conn)?
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
        Ok(dbs)
    })
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
pub async fn schema_databases_add_handler(
    axum::Json(body): axum::Json<SchemaDbAddRequest>,
) -> axum::response::Response {
    db_ok(|| {
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
        let conn = brain_utils::db::open_default()?;
        brain_utils::db::upsert_schema_database(&conn, &row)
    })
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
) -> axum::response::Response {
    db_remove("database", &name, || {
        let conn = brain_utils::db::open_default()?;
        brain_utils::db::remove_schema_database(&conn, &name)
    })
}
