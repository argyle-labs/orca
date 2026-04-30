use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

fn shopify_admin_version() -> String {
    #[derive(Deserialize, Default)]
    struct SpecsSection { shopify_admin_version: Option<String> }
    #[derive(Deserialize, Default)]
    struct BrainConfig { specs: Option<SpecsSection> }

    let home = std::env::var("HOME").unwrap_or_default();
    let toml_path = std::env::var("BRAIN_CONFIG")
        .unwrap_or_else(|_| format!("{home}/brain/config/brain.toml"));

    std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|raw| toml::from_str::<BrainConfig>(&raw).ok())
        .and_then(|cfg| cfg.specs?.shopify_admin_version)
        .unwrap_or_else(|| "2026-01".to_string())
}

use super::prelude::*;
pub use brain_scanner::{GraphQlEnum, GraphQlField, GraphQlInfo, GraphQlOperation, GraphQlType};

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
    let dir = specs_dir();

    // Optional registry.json provides metadata (project, description, base_url).
    // If absent, we auto-discover from *.json files in the directory.
    let registry: Vec<Value> = match std::fs::read_to_string(dir.join("registry.json")) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => vec![],
    };
    let mut by_repo: std::collections::HashMap<String, Value> = registry
        .into_iter()
        .filter_map(|e| {
            let repo = e.get("repo")?.as_str()?.to_string();
            Some((repo, e))
        })
        .collect();

    // Walk the specs dir and surface every *.json and *.graphql file.
    // .graphql-only repos (e.g. shopify-admin) have no .json counterpart — they
    // must still appear in the list so the UI can show the GraphQL viewer.
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Json::<Vec<Value>>(vec![]).into_response(),
    };
    let mut repos: Vec<String> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "registry.json" { return None; }
            // Accept .json (excluding .public.json) and .graphql
            if let Some(stem) = name.strip_suffix(".json") {
                if stem.ends_with(".public") { return None; }
                return Some(stem.to_string());
            }
            if let Some(stem) = name.strip_suffix(".graphql") {
                return Some(stem.to_string());
            }
            None
        })
        .collect();
    repos.sort();
    repos.dedup();

    let augmented: Vec<Value> = repos
        .into_iter()
        .map(|repo| {
            let mut entry = by_repo.remove(&repo).unwrap_or_else(|| {
                json!({
                    "repo": repo,
                    "project": repo,
                    "source": "manual",
                })
            });
            let has_full = dir.join(format!("{repo}.json")).exists();
            let has_public = dir.join(format!("{repo}.public.json")).exists();
            let has_graphql = dir.join(format!("{repo}.graphql")).exists();
            entry["hasGraphql"] = json!(has_graphql);
            entry["files"] = json!({
                "full":   if has_full   { Value::Bool(true) } else { Value::Null },
                "public": if has_public { Value::Bool(true) } else { Value::Null },
            });
            entry
        })
        .collect();
    Json(augmented).into_response()
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

// ── POST /api/specs/{repo}/graphql/proxy ─────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct GraphqlProxyRequest {
    /// Shopify shop domain (e.g. "myshop.myshopify.com" or "myshop")
    pub shop: String,
    /// Shopify Admin API access token
    pub token: String,
    /// GraphQL query or mutation document
    pub query: String,
    /// Query variables
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<Value>,
    /// Operation name
    #[serde(rename = "operationName", skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/specs/{repo}/graphql/proxy",
    operation_id = "proxyGraphql",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. shopify-admin)"),
    ),
    request_body = GraphqlProxyRequest,
    responses(
        (status = 200, description = "GraphQL response JSON from the upstream shop"),
        (status = 400, description = "Invalid repo name or request body", body = ErrorResponse),
        (status = 502, description = "Upstream request failed", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_graphql_proxy_handler(
    Path(repo): Path<String>,
    Json(body): Json<GraphqlProxyRequest>,
) -> Response {
    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }

    let version = shopify_admin_version();
    let shop = body.shop.trim().trim_end_matches('/');
    let shop_domain = if shop.contains('.') {
        shop.to_string()
    } else {
        format!("{shop}.myshopify.com")
    };
    let url = format!("https://{shop_domain}/admin/api/{version}/graphql.json");

    let mut payload = json!({ "query": body.query });
    if let Some(vars) = body.variables {
        payload["variables"] = vars;
    }
    if let Some(op) = body.operation_name {
        payload["operationName"] = Value::String(op);
    }

    let client = reqwest::Client::new();
    match client.post(&url)
        .header("X-Shopify-Access-Token", &body.token)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = resp.bytes().await.unwrap_or_default();
            axum::http::Response::builder()
                .status(status)
                .header(axum::http::header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(axum::body::Body::from(bytes))
                .unwrap()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, &format!("proxy error: {e}")),
    }
}

// ── GET /api/specs/{repo}/graphql/info ────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/graphql/info",
    operation_id = "getSpecGraphqlInfo",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
    ),
    responses(
        (status = 200, description = "Parsed GraphQL schema — types, queries, mutations, subscriptions", body = GraphQlInfo),
        (status = 400, description = "Invalid repo name", body = ErrorResponse),
        (status = 404, description = "GraphQL schema not found", body = ErrorResponse),
        (status = 422, description = "SDL parse error", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_graphql_info_handler(Path(repo): Path<String>) -> Response {
    if !validate_repo(&repo) {
        return err(StatusCode::BAD_REQUEST, "invalid repo name");
    }
    let path = specs_dir().join(format!("{repo}.graphql"));
    let sdl = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return err(StatusCode::NOT_FOUND, &format!("no GraphQL schema for '{repo}'")),
    };
    match brain_scanner::parse_graphql_sdl(&repo, &sdl) {
        Ok(info) => Json(info).into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    }
}
