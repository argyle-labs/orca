use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use db;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;

fn shopify_admin_version() -> String {
    #[derive(Deserialize, Default)]
    struct SpecsSection {
        shopify_admin_version: Option<String>,
    }
    #[derive(Deserialize, Default)]
    struct OrcaConfig {
        specs: Option<SpecsSection>,
    }

    let home = std::env::var("HOME").unwrap_or_default();
    let toml_path =
        std::env::var("ORCA_CONFIG").unwrap_or_else(|_| format!("{home}/.orca/orca.toml"));

    std::fs::read_to_string(&toml_path)
        .ok()
        .and_then(|raw| toml::from_str::<OrcaConfig>(&raw).ok())
        .and_then(|cfg| cfg.specs?.shopify_admin_version)
        .unwrap_or_else(|| "2026-01".to_string())
}

use super::prelude::*;
pub use crate::scanner::{GraphQlEnum, GraphQlField, GraphQlInfo, GraphQlOperation, GraphQlType};

// ── External spec registry ────────────────────────────────────────────────────
// Orca's own spec lives at /api/openapi.json and /api/openapi/public.json.
// These endpoints serve specs for *external* repos (rebuy and others) that are
// manually captured and stored in ~/.orca/openapi/.

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

use super::{specs_dir, validate_repo};

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

    builder
        .body(axum::body::Body::from(body))
        .expect("hardcoded headers are valid")
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
            if name == "registry.json" {
                return None;
            }
            // Accept .json (excluding .public.json) and .graphql
            if let Some(stem) = name.strip_suffix(".json") {
                if stem.ends_with(".public") {
                    return None;
                }
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

    let mut augmented: Vec<Value> = repos
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

    // Disk specs belong to the "orca" namespace
    for entry in &mut augmented {
        if entry.get("namespace").is_none() {
            entry["namespace"] = json!("orca");
        }
    }

    // Append DB-registered specs (URL-fetched / MCP-synced)
    if let Ok(conn) = db::open_default() {
        if let Ok(db_specs) = db::openapi_specs::list(&conn) {
            let disk_names: std::collections::HashSet<String> = augmented
                .iter()
                .filter_map(|e| e["repo"].as_str().map(|s| s.to_string()))
                .collect();
            for s in db_specs {
                if !disk_names.contains(&s.name) {
                    let path_count = s
                        .spec_json
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .and_then(|v| v["paths"].as_object().map(|p| p.len() as u64));
                    // source_mcp names the plugin whose MCP provided this spec (e.g. "rebuy")
                    let namespace = s.source_mcp.as_deref().unwrap_or("orca");
                    augmented.push(json!({
                        "repo": s.name,
                        "project": s.name,
                        "source": if s.source_mcp.is_some() { "mcp" } else { "url" },
                        "namespace": namespace,
                        "sourceMcp": s.source_mcp,
                        "baseUrl": s.url,
                        "capturedAt": s.cached_at,
                        "pathCount": path_count,
                        "hasGraphql": false,
                        "files": { "full": true, "public": null },
                    }));
                }
            }
        }

        // Append specs from plugin-declared spec directories (e.g. rebuy → ~/code/rebuy/rebuy-docs/docs/gen).
        // No dedup against orca-namespace specs: same filename in two namespaces is intentional —
        // orca's scanner and the plugin's own tooling may both cover the same service.
        if let Ok(plugins) = db::plugins::list(&conn) {
            for plugin in plugins
                .iter()
                .filter(|p| p.specs_dir.is_some() && p.enabled)
            {
                let plugin_dir = std::path::PathBuf::from(plugin.specs_dir.as_deref().unwrap());
                let Ok(read) = std::fs::read_dir(&plugin_dir) else {
                    continue;
                };
                // Track repos already added for THIS plugin to avoid intra-plugin dupes.
                let mut seen_in_plugin: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                let mut plugin_repos: Vec<String> = read
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if let Some(stem) = name.strip_suffix(".json") {
                            if stem.ends_with(".public") {
                                return None;
                            }
                            return Some(stem.to_string());
                        }
                        if let Some(stem) = name.strip_suffix(".graphql") {
                            return Some(stem.to_string());
                        }
                        None
                    })
                    .collect();
                plugin_repos.sort();
                plugin_repos.dedup();
                for repo in plugin_repos {
                    if !seen_in_plugin.insert(repo.clone()) {
                        continue;
                    }
                    let has_full = plugin_dir.join(format!("{repo}.json")).exists();
                    let has_public = plugin_dir.join(format!("{repo}.public.json")).exists();
                    let has_graphql = plugin_dir.join(format!("{repo}.graphql")).exists();
                    augmented.push(json!({
                        "repo": repo,
                        "project": repo,
                        "source": "plugin",
                        "namespace": plugin.id,
                        "hasGraphql": has_graphql,
                        "files": {
                            "full":   if has_full   { Value::Bool(true) } else { Value::Null },
                            "public": if has_public { Value::Bool(true) } else { Value::Null },
                        },
                    }));
                }
            }
        }
    }

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
    // 1. Orca's own spec dir (~/.orca/specs/)
    let path = specs_dir().join(format!("{repo}.json"));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        return serve_spec(&raw, &repo, &query);
    }
    // 2. DB-cached spec (URL-fetched or MCP-synced)
    if let Ok(conn) = db::open_default() {
        if let Ok(Some(row)) = db::openapi_specs::get(&conn, &repo)
            && let Some(raw) = row.spec_json
        {
            return serve_spec(&raw, &repo, &query);
        }
        // 3. Plugin-declared spec dirs
        if let Ok(plugins) = db::plugins::list(&conn) {
            for plugin in plugins
                .iter()
                .filter(|p| p.specs_dir.is_some() && p.enabled)
            {
                let plugin_path = std::path::PathBuf::from(plugin.specs_dir.as_deref().unwrap())
                    .join(format!("{repo}.json"));
                if let Ok(raw) = std::fs::read_to_string(&plugin_path) {
                    return serve_spec(&raw, &repo, &query);
                }
            }
        }
    }
    // Spec file missing — attempt a background sync via the orca CLI.
    // The CLI is authoritative on which repos are syncable; if unsupported it exits non-zero
    // and the next request will still 404. Return 202 so the client can retry.
    let exe = std::env::current_exe().unwrap_or_else(|_| "orca".into());
    let repo_clone = repo.clone();
    tokio::spawn(async move {
        let _ = tokio::process::Command::new(&exe)
            .args(["spec", "sync", &repo_clone])
            .output()
            .await;
    });
    (
        StatusCode::ACCEPTED,
        [("content-type", "application/json")],
        format!(r#"{{"generating":true,"repo":"{repo}","message":"Spec not found — generating now, retry in a few seconds"}}"#),
    )
        .into_response()
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
            &format!("no public spec for '{repo}' — create {repo}.public.json in ~/.orca/openapi/"),
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
            builder
                .body(axum::body::Body::from(sdl))
                .expect("hardcoded headers are valid")
        }
        Err(_) => err(
            StatusCode::NOT_FOUND,
            &format!("no GraphQL schema for '{repo}' — create {repo}.graphql in ~/.orca/openapi/"),
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
    match client
        .post(&url)
        .header("X-Shopify-Access-Token", &body.token)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = resp.bytes().await.unwrap_or_default();
            axum::http::Response::builder()
                .status(status)
                .header(
                    axum::http::header::CONTENT_TYPE,
                    "application/json; charset=utf-8",
                )
                .body(axum::body::Body::from(bytes))
                .expect("hardcoded headers are valid")
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
        Err(_) => {
            return err(
                StatusCode::NOT_FOUND,
                &format!("no GraphQL schema for '{repo}'"),
            );
        }
    };
    match crate::scanner::parse_graphql_sdl(&repo, &sdl) {
        Ok(info) => Json(info).into_response(),
        Err(e) => err(StatusCode::UNPROCESSABLE_ENTITY, &e.to_string()),
    }
}

// ── POST /api/specs/register ─────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/specs/register",
    operation_id = "registerSpec",
    request_body = SpecRegisterRequest,
    responses(
        (status = 200, description = "Spec fetched and stored", body = SpecInfo),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 502, description = "Failed to fetch spec from URL", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_register_handler(Json(body): Json<SpecRegisterRequest>) -> Response {
    if body.name.is_empty() || body.url.is_empty() {
        return err(StatusCode::BAD_REQUEST, "name and url are required");
    }
    let resp = match reqwest::get(&body.url).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("fetch failed: {e}")),
    };
    if !resp.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, &format!("HTTP {}", resp.status()));
    }
    let spec_json: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("invalid JSON: {e}")),
    };
    let spec_text = match serde_json::to_string(&spec_json) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let path_count = spec_json["paths"].as_object().map(|p| p.len() as u32);
    let cached_at = chrono::Utc::now().to_rfc3339();
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let row = db::openapi_specs::OpenApiSpecRow {
        name: body.name.clone(),
        url: Some(body.url.clone()),
        source_mcp: None,
        spec_json: Some(spec_text),
        cached_at: Some(cached_at.clone()),
        enabled: true,
    };
    if let Err(e) = db::openapi_specs::upsert(&conn, &row) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(SpecInfo {
        name: body.name,
        url: Some(body.url),
        source_mcp: None,
        path_count,
        cached_at: Some(cached_at),
        enabled: true,
    })
    .into_response()
}

// ── POST /api/specs/{name}/refresh ────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/specs/{name}/refresh",
    operation_id = "refreshSpec",
    params(
        ("name" = String, Path, description = "Spec name to refresh"),
    ),
    responses(
        (status = 200, description = "Spec refreshed from stored URL", body = SpecInfo),
        (status = 404, description = "Spec not found or has no URL", body = ErrorResponse),
        (status = 502, description = "Failed to fetch spec from URL", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_refresh_handler(Path(name): Path<String>) -> Response {
    if !validate_repo(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid spec name");
    }
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let row = match db::openapi_specs::get(&conn, &name) {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, &format!("no spec named '{name}'")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let url = match &row.url {
        Some(u) => u.clone(),
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("spec '{name}' has no URL — cannot refresh"),
            );
        }
    };
    let resp = match reqwest::get(&url).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("fetch failed: {e}")),
    };
    if !resp.status().is_success() {
        return err(StatusCode::BAD_GATEWAY, &format!("HTTP {}", resp.status()));
    }
    let spec_json: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("invalid JSON: {e}")),
    };
    let spec_text = match serde_json::to_string(&spec_json) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let path_count = spec_json["paths"].as_object().map(|p| p.len() as u32);
    let cached_at = chrono::Utc::now().to_rfc3339();
    let updated = db::openapi_specs::OpenApiSpecRow {
        name: row.name.clone(),
        url: row.url.clone(),
        source_mcp: row.source_mcp.clone(),
        spec_json: Some(spec_text),
        cached_at: Some(cached_at.clone()),
        enabled: row.enabled,
    };
    if let Err(e) = db::openapi_specs::upsert(&conn, &updated) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(SpecInfo {
        name: row.name,
        url: row.url,
        source_mcp: row.source_mcp,
        path_count,
        cached_at: Some(cached_at),
        enabled: row.enabled,
    })
    .into_response()
}

// ── DELETE /api/specs/{name}/unregister ──────────────────────────────────────

#[utoipa::path(
    delete,
    path = "/api/specs/{name}/unregister",
    operation_id = "unregisterSpec",
    params(
        ("name" = String, Path, description = "Spec name to unregister"),
    ),
    responses(
        (status = 200, description = "Spec removed from DB", body = OkResponse),
        (status = 404, description = "Spec not found", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_unregister_handler(Path(name): Path<String>) -> Response {
    if !validate_repo(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid spec name");
    }
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    match db::openapi_specs::remove(&conn, &name) {
        Ok(true) => Json(OkResponse { ok: true }).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, &format!("no spec named '{name}'")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── POST /api/specs/sync-mcp/{server} ────────────────────────────────────────
// Calls the named MCP server's `{server}_spec_list` + `{server}_spec_schema`
// tools and upserts the results into orca.db tagged with source_mcp = server.

#[utoipa::path(
    post,
    path = "/api/specs/sync-mcp/{server}",
    operation_id = "syncMcpSpecs",
    params(
        ("server" = String, Path, description = "MCP server name (e.g. rebuy-cli)")
    ),
    responses(
        (status = 200, description = "Sync result", body = OkResponse),
    ),
    tag = "specs"
)]
pub async fn specs_sync_mcp_handler(
    State(pool): State<super::McpState>,
    Path(server): Path<String>,
) -> Response {
    // Derive tool name prefix from server name: "rebuy-cli" → "rebuy"
    let prefix = server.split('-').next().unwrap_or(&server).to_string();
    let list_tool = format!("{prefix}_spec_list");

    let client = match pool.get_or_connect(&server).await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("MCP connect failed: {e}")),
    };

    let list_result = match client.call_tool(&list_tool, json!({}), "sync-mcp").await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_GATEWAY, &format!("{list_tool} failed: {e}")),
    };

    // The result is an array of content items; extract the text
    let text = list_result["content"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find_map(|c| c["text"].as_str().map(|s| s.to_string()))
        })
        .unwrap_or_default();

    // Parse JSON array of spec entries or newline-delimited repo names
    let repos: Vec<String> = if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) {
        arr.into_iter()
            .filter_map(|v| {
                v["repo"]
                    .as_str()
                    .or_else(|| v["name"].as_str())
                    .or_else(|| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    } else {
        text.lines()
            .map(|l| {
                l.trim()
                    .trim_start_matches("• ")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .filter(|s| !s.is_empty() && !s.contains(':'))
            .collect()
    };

    if repos.is_empty() {
        return err(StatusCode::BAD_GATEWAY, "MCP spec list returned no repos");
    }

    let schema_tool = format!("{prefix}_spec_schema");
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let mut synced = 0usize;
    let mut errors: Vec<String> = vec![];

    for repo in &repos {
        if repo.is_empty() {
            continue;
        }
        let result = client
            .call_tool(&schema_tool, json!({ "repo": repo }), "sync-mcp")
            .await;
        match result {
            Err(e) => {
                errors.push(format!("{repo}: {e}"));
                continue;
            }
            Ok(r) => {
                let spec_text = r["content"].as_array().and_then(|arr| {
                    arr.iter()
                        .find_map(|c| c["text"].as_str().map(|s| s.to_string()))
                });
                let Some(spec_text) = spec_text else {
                    errors.push(format!("{repo}: empty schema response"));
                    continue;
                };
                // Validate it's JSON
                if serde_json::from_str::<Value>(&spec_text).is_err() {
                    errors.push(format!("{repo}: non-JSON schema"));
                    continue;
                }
                let row = db::openapi_specs::OpenApiSpecRow {
                    name: repo.clone(),
                    url: None,
                    source_mcp: Some(prefix.clone()),
                    spec_json: Some(spec_text),
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    enabled: true,
                };
                match db::openapi_specs::upsert(&conn, &row) {
                    Ok(_) => synced += 1,
                    Err(e) => errors.push(format!("{repo}: db error: {e}")),
                }
            }
        }
    }

    Json(json!({
        "ok": true,
        "server": server,
        "synced": synced,
        "errors": errors,
    }))
    .into_response()
}

// ── GET /api/specs/db ─────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/specs/db",
    operation_id = "listDbSpecs",
    responses(
        (status = 200, description = "All URL-registered specs from orca.db", body = Vec<SpecInfo>),
    ),
    tag = "specs"
)]
pub async fn specs_db_list_handler() -> Response {
    let conn = match db::open_default() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let rows = match db::openapi_specs::list(&conn) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let infos: Vec<SpecInfo> = rows
        .into_iter()
        .map(|r| {
            let path_count = r
                .spec_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|v| v["paths"].as_object().map(|p| p.len() as u32));
            SpecInfo {
                name: r.name,
                url: r.url,
                source_mcp: r.source_mcp,
                path_count,
                cached_at: r.cached_at,
                enabled: r.enabled,
            }
        })
        .collect();
    Json(infos).into_response()
}
