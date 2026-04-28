use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

use super::prelude::*;

// ── External spec registry ────────────────────────────────────────────────────
// Brain's own spec lives at /api/openapi.json and /api/openapi/public.json.
// These endpoints serve specs for *external* repos (rebuy and others) that are
// manually captured and stored in ~/brain/openapi/.

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct SpecFiles {
    pub full: Option<String>,
    pub public: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct SpecMeta {
    pub repo: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: String,
    #[serde(rename = "baseUrl", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(rename = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(rename = "pathCount", skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<SpecFiles>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct SpecQuery {
    /// "json" (default) or "yaml"
    pub format: Option<String>,
    /// true adds Content-Disposition: attachment header
    pub download: Option<bool>,
}

fn specs_dir() -> std::path::PathBuf {
    brain_scanner::openapi_dir()
}

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn serve_spec(raw: &str, filename_base: &str, query: &SpecQuery) -> Response {
    use axum::http::header;
    let want_yaml = query.format.as_deref() == Some("yaml");

    let (body, content_type, ext) = if want_yaml {
        let converted = serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|v| serde_yaml::to_string(&v).ok());
        match converted {
            Some(yaml) => (yaml, "application/yaml; charset=utf-8", "yaml"),
            None => (raw.to_string(), "application/json; charset=utf-8", "json"),
        }
    } else {
        (raw.to_string(), "application/json; charset=utf-8", "json")
    };

    let mut builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type);

    if query.download.unwrap_or(false) {
        let disposition = format!("attachment; filename=\"{filename_base}.{ext}\"");
        builder = builder.header(header::CONTENT_DISPOSITION, disposition);
    }

    builder.body(axum::body::Body::from(body)).unwrap()
}

#[utoipa::path(
    get,
    path = "/api/specs",
    operation_id = "listSpecs",
    responses(
        (status = 200, description = "All registered external specs", body = Vec<SpecMeta>),
    ),
    tag = "specs"
)]
pub async fn specs_list_handler() -> Response {
    let path = specs_dir().join("registry.json");
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => Json(v).into_response(),
            Err(_) => Json(json!([])).into_response(),
        },
        Err(_) => Json(json!([])).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/api/specs/{repo}",
    operation_id = "getSpec",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
        ("format" = Option<String>, Query, description = "Response format: json (default) or yaml"),
        ("download" = Option<bool>, Query, description = "Set true to receive Content-Disposition: attachment"),
    ),
    responses(
        (status = 200, description = "Full OpenAPI spec for the repo"),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "Spec not found", body = ErrorResponse),
        (status = 500, description = "Invalid spec JSON", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_get_handler(
    Path(repo): Path<String>,
    Query(query): Query<SpecQuery>,
) -> Response {
    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }
    let path = specs_dir().join(format!("{repo}.json"));
    match std::fs::read_to_string(&path) {
        Ok(raw) => serve_spec(&raw, &repo, &query),
        Err(_) => err(
            StatusCode::NOT_FOUND,
            &format!("no spec registered for '{repo}'"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/public",
    operation_id = "getSpecPublic",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
        ("format" = Option<String>, Query, description = "Response format: json (default) or yaml"),
        ("download" = Option<bool>, Query, description = "Set true to receive Content-Disposition: attachment"),
    ),
    responses(
        (status = 200, description = "Public-tagged operations only"),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "Spec not found", body = ErrorResponse),
        (status = 500, description = "Invalid spec JSON", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_get_public_handler(
    Path(repo): Path<String>,
    Query(query): Query<SpecQuery>,
) -> Response {
    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }
    let path = specs_dir().join(format!("{repo}.public.json"));
    match std::fs::read_to_string(&path) {
        Ok(raw) => serve_spec(&raw, &format!("{repo}.public"), &query),
        Err(_) => err(
            StatusCode::NOT_FOUND,
            &format!("no public spec for '{repo}' — create {repo}.public.json in ~/brain/openapi/"),
        ),
    }
}

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/graphql",
    operation_id = "getSpecGraphql",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
        ("download" = Option<bool>, Query, description = "Set true to receive Content-Disposition: attachment"),
    ),
    responses(
        (status = 200, description = "GraphQL SDL schema for the repo", content_type = "text/plain"),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "Schema not found", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_get_graphql_handler(
    Path(repo): Path<String>,
    Query(query): Query<SpecQuery>,
) -> Response {
    use axum::http::header;
    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }
    let path = specs_dir().join(format!("{repo}.graphql"));
    match std::fs::read_to_string(&path) {
        Ok(sdl) => {
            let mut builder = axum::http::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8");
            if query.download.unwrap_or(false) {
                builder = builder.header(
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{repo}.graphql\""),
                );
            }
            builder.body(axum::body::Body::from(sdl)).unwrap()
        }
        Err(_) => err(
            StatusCode::NOT_FOUND,
            &format!("no GraphQL schema for '{repo}' — create {repo}.graphql in ~/brain/openapi/"),
        ),
    }
}
