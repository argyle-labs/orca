use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;
use serde_json::Value;
use utoipa::ToSchema;

use super::prelude::*;

fn specs_dir() -> std::path::PathBuf {
    brain_scanner::openapi_dir()
}

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[derive(Deserialize, ToSchema)]
pub struct SpecDownloadQuery {
    /// json (default) or yaml
    pub format: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct GraphqlDownloadQuery {
    /// sdl (default) or introspection
    pub format: Option<String>,
}

// ── GET /api/specs/:repo/download ─────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/download",
    operation_id = "downloadSpec",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
        ("format" = Option<String>, Query, description = "json (default) or yaml"),
    ),
    responses(
        (status = 200, description = "OpenAPI spec file download"),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "Spec not found", body = ErrorResponse),
        (status = 500, description = "Invalid spec JSON or YAML conversion failed", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn spec_download_handler(
    Path(repo): Path<String>,
    Query(params): Query<SpecDownloadQuery>,
) -> Response {
    use axum::http::header;

    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }

    let path = specs_dir().join(format!("{repo}.json"));
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return err(StatusCode::NOT_FOUND, &format!("no spec registered for '{repo}'")),
    };

    let want_yaml = params.format.as_deref() == Some("yaml");

    let (body, content_type, ext) = if want_yaml {
        let converted = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| serde_yaml::to_string(&v).ok());
        match converted {
            Some(yaml) => (yaml, "application/yaml; charset=utf-8", "yaml"),
            None => return err(StatusCode::INTERNAL_SERVER_ERROR, "yaml conversion failed"),
        }
    } else {
        (raw, "application/json; charset=utf-8", "json")
    };

    let disposition = format!("attachment; filename=\"{repo}-openapi.{ext}\"");

    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(body))
        .unwrap()
}

// ── GET /api/specs/:repo/graphql/download ─────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/graphql/download",
    operation_id = "downloadGraphql",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
        ("format" = Option<String>, Query, description = "sdl (default) or introspection"),
    ),
    responses(
        (status = 200, description = "GraphQL schema download"),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "Schema not found", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn graphql_download_handler(
    Path(repo): Path<String>,
    Query(params): Query<GraphqlDownloadQuery>,
) -> Response {
    use axum::http::header;

    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }

    let path = specs_dir().join(format!("{repo}.graphql"));
    let sdl = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            return err(
                StatusCode::NOT_FOUND,
                &format!("no GraphQL schema for '{repo}'"),
            )
        }
    };

    let want_introspection = params.format.as_deref() == Some("introspection");

    if want_introspection {
        let payload = serde_json::json!({ "data": { "__schema": { "sdl": sdl } } });
        let body = serde_json::to_string_pretty(&payload).unwrap_or_default();
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{repo}-schema-introspection.json\""),
            )
            .body(axum::body::Body::from(body))
            .unwrap()
    } else {
        axum::http::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{repo}-schema.graphql\""),
            )
            .body(axum::body::Body::from(sdl))
            .unwrap()
    }
}
