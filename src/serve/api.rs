use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use utoipa::ToSchema;

use super::mcp_client::McpPool;
use super::tree::{build_tree_raw, collect_all_files, get_root_tree, get_roots, get_search_ignored};

pub type McpState = Arc<McpPool>;

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(ErrorResponse { error: msg.to_string() })).into_response()
}

// ── Shared response schemas ───────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize, ToSchema)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<TreeNode>>,
}

#[derive(Serialize, ToSchema)]
pub struct SearchResult {
    pub root: String,
    pub path: String,
    pub matches: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct McpRunRequest {
    pub server: String,
    pub name: String,
    pub arguments: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct McpRunResponse {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[derive(Serialize, ToSchema)]
pub struct DockerService {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DockerServicesResponse {
    #[serde(rename = "composeFile")]
    pub compose_file: Option<String>,
    pub services: Vec<DockerService>,
}

#[derive(Deserialize, Serialize, ToSchema)]
pub struct DockerActionRequest {
    #[serde(rename = "projectPath")]
    pub project_path: String,
    pub service: Option<String>,
    pub action: String,
    pub tail: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub struct DockerActionResponse {
    pub output: String,
    #[serde(rename = "composeFile")]
    pub compose_file: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Ctx7Response {
    #[serde(rename = "libraryId")]
    pub library_id: String,
    pub title: String,
    pub topic: Option<String>,
    pub content: String,
}

#[derive(Serialize, ToSchema)]
pub struct SchemaResponse {
    pub tabs: Vec<SchemaTab>,
    #[serde(rename = "showTabs")]
    pub show_tabs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
}

#[derive(Serialize, ToSchema)]
pub struct SchemaTab {
    pub title: String,
    pub tables: Vec<Value>,
    pub columns: Value,
    #[serde(rename = "foreignKeys")]
    pub foreign_keys: Vec<Value>,
    pub domains: Value,
}

#[derive(Serialize, ToSchema)]
pub struct HealthCheck {
    pub label: String,
    pub tool: String,
    pub output: String,
    pub ok: bool,
}

#[derive(Serialize, ToSchema)]
pub struct HealthResponse {
    pub timestamp: String,
    pub checks: Vec<HealthCheck>,
}

#[derive(Serialize, ToSchema)]
pub struct LogService {
    pub name: String,
    pub state: String,
    pub running: bool,
    pub health: String,
    pub ports: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct LogProject {
    pub project: String,
    pub path: String,
    pub services: Vec<LogService>,
}

#[derive(Serialize, ToSchema)]
pub struct LogServicesResponse {
    pub projects: Vec<LogProject>,
}

#[derive(Serialize, ToSchema)]
pub struct LogsResponse {
    pub output: String,
}

// ── GET /api/tree ─────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/tree",
    operation_id = "getTree",
    responses(
        (status = 200, description = "Document tree indexed by root name", body = Value),
        (status = 500, description = "Error", body = ErrorResponse),
    ),
    tag = "docs"
)]
pub async fn tree_handler() -> impl IntoResponse {
    let mut result = HashMap::new();
    for name in ["rebuy", "brain"] {
        result.insert(name, get_root_tree(name));
    }
    Json(result)
}

// ── GET /api/search ───────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub root: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/search",
    operation_id = "searchDocs",
    params(
        ("q" = Option<String>, Query, description = "Search query"),
        ("root" = Option<String>, Query, description = "Limit search to a specific root (brain/rebuy)"),
    ),
    responses(
        (status = 200, description = "Search results", body = Vec<SearchResult>),
    ),
    tag = "docs"
)]
pub async fn search_handler(Query(params): Query<SearchQuery>) -> Response {
    let query = params.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Json(json!([])).into_response();
    }
    let root_filter = params.root.as_deref().unwrap_or("all");
    let roots = get_roots();
    let pattern = regex::escape(&query).to_lowercase();
    let mut results: Vec<Value> = Vec::new();

    for (name, root_dir) in &roots {
        if root_filter != "all" && root_filter != name {
            continue;
        }
        let ignored = get_search_ignored(name);
        let tree = build_tree_raw(root_dir, root_dir, &ignored);
        let files = collect_all_files(&tree);
        for file in files {
            let full = root_dir.join(&file.path);
            if let Ok(content) = std::fs::read_to_string(&full) {
                let matches: Vec<String> = content
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&pattern))
                    .take(3)
                    .map(|(i, line)| format!("L{}: {}", i + 1, line.trim()))
                    .collect();
                if !matches.is_empty() {
                    let file_path = file
                        .path
                        .replace(".md", "")
                        .replace(".mdx", "")
                        .replace('\\', "/");
                    results.push(json!({ "root": name, "path": file_path, "matches": matches }));
                }
            }
        }
    }
    results.sort_by(|a, b| {
        let am = a["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        let bm = b["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        bm.cmp(&am)
    });
    results.truncate(20);
    Json(results).into_response()
}

// ── GET /api/mcp/tools ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/mcp/tools",
    operation_id = "getMcpTools",
    responses(
        (status = 200, description = "All tools from all connected MCP servers", body = Vec<McpToolInfo>),
    ),
    tag = "mcp"
)]
pub async fn mcp_tools_handler(State(pool): State<McpState>) -> impl IntoResponse {
    let tools = pool.all_tools().await;
    Json(tools)
}

// ── POST /api/mcp/run ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/mcp/run",
    operation_id = "runMcpTool",
    request_body = McpRunRequest,
    responses(
        (status = 200, description = "Tool result", body = McpRunResponse),
        (status = 404, description = "Unknown MCP server", body = ErrorResponse),
        (status = 500, description = "Tool execution error", body = ErrorResponse),
    ),
    tag = "mcp"
)]
pub async fn mcp_run_handler(
    State(pool): State<McpState>,
    Json(body): Json<McpRunRequest>,
) -> Response {
    match pool.get_or_connect(&body.server).await {
        Err(e) => err(StatusCode::NOT_FOUND, &e.to_string()),
        Ok(client) => {
            let args = body.arguments.unwrap_or(json!({}));
            match client.call_tool(&body.name, args).await {
                Ok(result) => Json(result).into_response(),
                Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
    }
}

// ── GET /api/doc ──────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct DocQuery {
    pub root: String,
    pub path: String,
}

#[utoipa::path(
    get,
    path = "/api/doc",
    operation_id = "getDoc",
    params(
        ("root" = String, Query, description = "Vault root name (brain/rebuy)"),
        ("path" = String, Query, description = "File path relative to root"),
    ),
    responses(
        (status = 200, description = "Document content as plain text", content_type = "text/plain"),
        (status = 400, description = "Unknown root", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse),
    ),
    tag = "docs"
)]
pub async fn doc_handler(Query(params): Query<DocQuery>) -> Response {
    let roots = get_roots();
    let Some(root_dir) = roots.get(&params.root) else {
        return err(StatusCode::BAD_REQUEST, "unknown root");
    };
    let full = root_dir.join(&params.path);
    if !full.starts_with(root_dir) {
        return err(StatusCode::FORBIDDEN, "path traversal");
    }
    match std::fs::read_to_string(&full) {
        Ok(content) => (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "not found"),
    }
}

// ── GET /api/docker/services ──────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct DockerServicesQuery {
    pub path: String,
}

#[utoipa::path(
    get,
    path = "/api/docker/services",
    operation_id = "getDockerServices",
    params(
        ("path" = String, Query, description = "Absolute path to the Docker Compose project directory"),
    ),
    responses(
        (status = 200, description = "Compose file path and service list", body = DockerServicesResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_services_handler(Query(params): Query<DockerServicesQuery>) -> Response {
    let compose_file = match find_compose_file(&params.path) {
        Some(f) => f,
        None => {
            return Json(json!({ "services": [], "composeFile": null })).into_response()
        }
    };

    let names_out = match run_docker(
        &["compose", "-f", compose_file.to_str().unwrap(), "config", "--services"],
        None,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let names: Vec<String> = names_out
        .trim()
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let ps_raw = run_docker(
        &["compose", "-f", compose_file.to_str().unwrap(), "ps", "--format", "json"],
        None,
    )
    .await
    .unwrap_or_default();
    let statuses = parse_compose_ps(&ps_raw);

    let services: Vec<Value> = names
        .iter()
        .map(|name| {
            let s = statuses.get(name.as_str()).cloned().unwrap_or_default();
            json!({
                "name": name,
                "state": s.state,
                "running": s.state.to_lowercase().contains("running"),
                "health": s.health,
                "ports": s.ports,
            })
        })
        .collect();

    Json(json!({ "composeFile": compose_file, "services": services })).into_response()
}

// ── POST /api/docker/action ───────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/api/docker/action",
    operation_id = "runDockerAction",
    request_body = DockerActionRequest,
    responses(
        (status = 200, description = "Action output", body = DockerActionResponse),
        (status = 400, description = "Unknown action", body = ErrorResponse),
        (status = 404, description = "No compose file", body = ErrorResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "docker"
)]
pub async fn docker_action_handler(Json(body): Json<DockerActionRequest>) -> Response {
    let compose_file = match find_compose_file(&body.project_path) {
        Some(f) => f,
        None => return err(StatusCode::NOT_FOUND, "no compose file found"),
    };
    let cf = compose_file.to_str().unwrap().to_string();
    let svc: Vec<String> = body.service.as_deref().map(|s| vec![s.to_string()]).unwrap_or_default();
    let tail_str;

    let args: Vec<String> = match body.action.as_str() {
        "start" => { let mut a = vec!["compose".into(), "-f".into(), cf, "start".into()]; a.extend(svc); a }
        "stop" => { let mut a = vec!["compose".into(), "-f".into(), cf, "stop".into()]; a.extend(svc); a }
        "restart" => { let mut a = vec!["compose".into(), "-f".into(), cf, "restart".into()]; a.extend(svc); a }
        "up" => { let mut a = vec!["compose".into(), "-f".into(), cf, "up".into(), "-d".into()]; a.extend(svc); a }
        "down" => {
            if let Some(ref s) = body.service {
                vec!["compose".into(), "-f".into(), cf, "stop".into(), s.clone()]
            } else {
                vec!["compose".into(), "-f".into(), cf, "down".into()]
            }
        }
        "build" => { let mut a = vec!["compose".into(), "-f".into(), cf, "build".into(), "--no-cache".into()]; a.extend(svc); a }
        "pull" => { let mut a = vec!["compose".into(), "-f".into(), cf, "pull".into()]; a.extend(svc); a }
        "logs" => {
            tail_str = body.tail.unwrap_or(100).to_string();
            let mut a = vec!["compose".into(), "-f".into(), cf, "logs".into(), "--tail".into(), tail_str.clone(), "--no-color".into()];
            a.extend(svc);
            a
        }
        "ps" => vec!["compose".into(), "-f".into(), cf, "ps".into(), "--format".into(), "json".into()],
        _ => return err(StatusCode::BAD_REQUEST, &format!("unknown action: {}", body.action)),
    };

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match run_docker(&args_ref, None).await {
        Ok(output) => Json(DockerActionResponse {
            output,
            compose_file: compose_file.to_str().map(|s| s.to_string()),
        }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── GET /api/ctx7 ─────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct Ctx7Query {
    pub q: String,
    pub topic: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/ctx7",
    operation_id = "getLibraryDocs",
    params(
        ("q" = String, Query, description = "Library name to look up (npm package, crate, etc.)"),
        ("topic" = Option<String>, Query, description = "Specific topic or function to focus on"),
    ),
    responses(
        (status = 200, description = "Library documentation", body = Ctx7Response),
        (status = 400, description = "Missing query", body = ErrorResponse),
        (status = 404, description = "Library not found", body = ErrorResponse),
        (status = 503, description = "context7 not available", body = ErrorResponse),
    ),
    tag = "library"
)]
pub async fn ctx7_handler(
    Query(params): Query<Ctx7Query>,
    State(pool): State<McpState>,
) -> Response {
    if params.q.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "q required");
    }
    let Some(server) = pool.find_ctx7_server().await else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "context7 not registered — add it to ~/.claude.json mcpServers");
    };
    let Ok(client) = pool.get_or_connect(&server).await else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "could not connect to context7");
    };

    let resolve_result = match client.call_tool("resolve-library-id", json!({ "libraryName": params.q })).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let resolve_text = resolve_result["content"][0]["text"].as_str().unwrap_or("").to_string();
    let Some((lib_id, lib_title)) = extract_library_id(&resolve_text) else {
        return err(StatusCode::NOT_FOUND, &format!("No library found for \"{}\"", params.q));
    };

    let mut docs_args = json!({ "context7CompatibleLibraryID": lib_id, "tokens": 8000 });
    if let Some(ref topic) = params.topic {
        docs_args["topic"] = json!(topic);
    }

    match client.call_tool("get-library-docs", docs_args).await {
        Ok(result) => {
            let content = result["content"][0]["text"].as_str().unwrap_or("").to_string();
            Json(Ctx7Response { library_id: lib_id, title: lib_title, topic: params.topic, content }).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── Schema database config ────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct DbConfig {
    name: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: u16,
    user: String,
    password: String,
    database: String,
    container: Option<String>,
    #[serde(alias = "domainsFile")]
    domains_file: Option<String>,
}

#[derive(Deserialize, Default)]
struct SchemaSection {
    databases: Vec<DbConfig>,
}

#[derive(Deserialize, Default)]
struct BrainConfig {
    schema: Option<SchemaSection>,
}

fn load_db_configs() -> Vec<DbConfig> {
    let home = std::env::var("HOME").unwrap_or_default();

    // 1. BRAIN_CONFIG env var override, or default brain.toml path
    let toml_path = std::env::var("BRAIN_CONFIG")
        .unwrap_or_else(|_| format!("{home}/brain/config/brain.toml"));

    if let Ok(raw) = std::fs::read_to_string(&toml_path) {
        if let Ok(cfg) = toml::from_str::<BrainConfig>(&raw) {
            let dbs = cfg.schema.map(|s| s.databases).unwrap_or_default();
            if !dbs.is_empty() {
                return dbs;
            }
        }
    }

    // 2. Legacy JSON fallback
    let json_path = format!("{home}/brain/config/schema-databases.json");
    let raw = std::fs::read_to_string(&json_path).unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or_default()
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

fn load_domains(domains_file: &Option<String>) -> Value {
    let Some(path) = domains_file else { return json!([]) };
    let expanded = expand_tilde(path);
    std::fs::read_to_string(&expanded)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(json!([]))
}

// ── GET /api/schema ───────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/schema",
    operation_id = "getSchema",
    responses(
        (status = 200, description = "Database schema tabs", body = SchemaResponse),
        (status = 404, description = "No databases configured", body = ErrorResponse),
        (status = 500, description = "All DB connections failed", body = ErrorResponse),
    ),
    tag = "schema"
)]
pub async fn schema_handler() -> Response {
    let configs = load_db_configs();
    if configs.is_empty() {
        return err(StatusCode::NOT_FOUND, "No databases configured — add [[schema.databases]] entries to ~/brain/config/brain.toml (or set BRAIN_CONFIG to a custom path)");
    }

    let mut tabs = Vec::new();
    let mut errors = Vec::new();

    for cfg in &configs {
        match query_database(cfg).await {
            Ok(tab) => tabs.push(tab),
            Err(e) => errors.push(format!("{}: {e}", cfg.name)),
        }
    }

    if tabs.is_empty() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &format!("All databases failed: {}", errors.join("; ")));
    }

    let show_tabs = tabs.len() > 1;
    let errors_opt = if errors.is_empty() { None } else { Some(errors) };
    Json(json!({ "tabs": tabs, "showTabs": show_tabs, "errors": errors_opt })).into_response()
}

async fn query_database(cfg: &DbConfig) -> anyhow::Result<Value> {
    let db = &cfg.database;
    let tables_sql = format!(
        "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA='{db}' AND TABLE_TYPE='BASE TABLE' ORDER BY TABLE_NAME"
    );
    let cols_sql = format!(
        "SELECT TABLE_NAME, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, COLUMN_KEY, EXTRA \
         FROM information_schema.COLUMNS WHERE TABLE_SCHEMA='{db}' ORDER BY TABLE_NAME, ORDINAL_POSITION"
    );
    let fk_sql = format!(
        "SELECT TABLE_NAME, COLUMN_NAME, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
         FROM information_schema.KEY_COLUMN_USAGE \
         WHERE TABLE_SCHEMA='{db}' AND REFERENCED_TABLE_NAME IS NOT NULL"
    );

    let (tables_raw, cols_raw, fk_raw) = tokio::try_join!(
        mysql_query_cfg(cfg, &tables_sql),
        mysql_query_cfg(cfg, &cols_sql),
        mysql_query_cfg(cfg, &fk_sql),
    )?;

    let tables: Vec<Value> = parse_mysql_tsv(&tables_raw, &["name", "comment"])
        .into_iter()
        .map(|mut row| {
            let name = row.remove("name").unwrap_or_default();
            json!({ "name": name, "comment": row.remove("comment").unwrap_or_default() })
        })
        .collect();

    let fk_rows = parse_mysql_tsv(&fk_raw, &["table", "column", "ref_table", "ref_column"]);
    let mut fk_lookup: HashMap<(String, String), String> = HashMap::new();
    for row in &fk_rows {
        let key = (row.get("table").cloned().unwrap_or_default(), row.get("column").cloned().unwrap_or_default());
        fk_lookup.insert(key, row.get("ref_table").cloned().unwrap_or_default());
    }

    let mut columns: HashMap<String, Vec<Value>> = HashMap::new();
    for row in parse_mysql_tsv(&cols_raw, &["table", "name", "type", "nullable", "key", "extra"]) {
        let table = row.get("table").cloned().unwrap_or_default();
        let col_name = row.get("name").cloned().unwrap_or_default();
        let fk_target = fk_lookup.get(&(table.clone(), col_name.clone())).cloned();
        columns.entry(table).or_default().push(json!({
            "name": col_name,
            "type": row.get("type"),
            "nullable": row.get("nullable") == Some(&"YES".to_string()),
            "key": row.get("key"),
            "extra": row.get("extra"),
            "fk_target": fk_target,
        }));
    }

    let foreign_keys: Vec<Value> = fk_rows
        .into_iter()
        .map(|row| json!({
            "table": row.get("table"),
            "column": row.get("column"),
            "refTable": row.get("ref_table"),
            "refColumn": row.get("ref_column"),
        }))
        .collect();

    let domains = load_domains(&cfg.domains_file);

    Ok(json!({
        "title": cfg.name,
        "tables": tables,
        "columns": columns,
        "foreignKeys": foreign_keys,
        "domains": domains,
    }))
}

async fn mysql_query_cfg(cfg: &DbConfig, sql: &str) -> anyhow::Result<String> {
    let pass_arg = format!("-p{}", cfg.password);
    let mysql_args = ["-u", cfg.user.as_str(), pass_arg.as_str(), cfg.database.as_str(), "--batch", "--silent", "-e", sql];

    let out = if let Some(container) = &cfg.container {
        let mut args = vec!["exec", container.as_str(), "mysql"];
        args.extend_from_slice(&mysql_args);
        Command::new("docker").args(&args).output().await?
    } else {
        let port_str = cfg.port.to_string();
        let mut args = vec!["-h", cfg.host.as_str(), "-P", port_str.as_str()];
        args.extend_from_slice(&mysql_args);
        Command::new("mysql").args(&args).output().await?
    };

    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("{}", e.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn parse_mysql_tsv(raw: &str, cols: &[&str]) -> Vec<HashMap<String, String>> {
    raw.lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let values: Vec<&str> = line.split('\t').collect();
            cols.iter().enumerate()
                .map(|(i, &col)| (col.to_string(), values.get(i).unwrap_or(&"").to_string()))
                .collect()
        })
        .collect()
}

// ── GET /api/rebuy/health/local ───────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/rebuy/health/local",
    operation_id = "getHealth",
    responses(
        (status = 200, description = "All service health checks", body = HealthResponse),
        (status = 503, description = "rebuy-cli MCP unavailable", body = ErrorResponse),
    ),
    tag = "health"
)]
pub async fn rebuy_health_handler(State(pool): State<McpState>) -> Response {
    const CHECKS: &[(&str, &str)] = &[
        ("DB",      "rebuy_db_status"),
        ("Env",     "rebuy_env_status"),
        ("Engines", "rebuy_engines_status"),
        ("Tunnel",  "rebuy_tunnel_status"),
        ("Network", "rebuy_network_status"),
        ("Mode",    "rebuy_mode_current"),
    ];

    let client = match pool.get_or_connect("rebuy-cli").await {
        Ok(c) => c,
        Err(e) => return err(StatusCode::SERVICE_UNAVAILABLE, &format!("rebuy-cli MCP unavailable: {e}")),
    };

    let futures: Vec<_> = CHECKS.iter().map(|(label, tool)| {
        let client = client.clone();
        let label = label.to_string();
        let tool = tool.to_string();
        async move {
            let result = client.call_tool(&tool, json!({})).await;
            let output = match &result {
                Ok(v) => v["content"][0]["text"].as_str().unwrap_or("").to_string(),
                Err(e) => format!("error: {e}"),
            };
            let ok = result.is_ok() && !output.to_lowercase().contains("error");
            HealthCheck { label, tool, output, ok }
        }
    }).collect();

    let checks = futures_util::future::join_all(futures).await;
    Json(HealthResponse { timestamp: chrono::Utc::now().to_rfc3339(), checks }).into_response()
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
    let configs = load_db_configs();
    let mut all: Vec<Value> = Vec::new();
    for cfg in &configs {
        if let Value::Array(domains) = load_domains(&cfg.domains_file) {
            all.extend(domains);
        }
    }
    Json(json!(all)).into_response()
}

// ── GET /api/logs/services ────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/logs/services",
    operation_id = "getLogServices",
    responses(
        (status = 200, description = "All Docker projects and their service states", body = LogServicesResponse),
        (status = 404, description = "Rebuy root not found", body = ErrorResponse),
    ),
    tag = "logs"
)]
pub async fn log_services_handler() -> Response {
    let home = std::env::var("HOME").unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT").unwrap_or_else(|_| format!("{home}/code/rebuy"));

    let project_dirs: Vec<PathBuf> = match std::fs::read_dir(&rebuy_root) {
        Err(_) => return err(StatusCode::NOT_FOUND, "rebuy root not found"),
        Ok(entries) => entries.flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.is_dir() && find_compose_file(p.to_str()?).is_some() { Some(p) } else { None }
            })
            .collect(),
    };

    let futures: Vec<_> = project_dirs.iter().map(|project_path| {
        let project_path = project_path.clone();
        async move {
            let path_str = project_path.to_string_lossy().to_string();
            let name = project_path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path_str.clone());
            let compose_file = match find_compose_file(&path_str) {
                Some(f) => f,
                None => return json!({ "project": name, "path": path_str, "services": [] }),
            };
            let ps_raw = run_docker(&["compose", "-f", compose_file.to_str().unwrap(), "ps", "--format", "json"], None).await.unwrap_or_default();
            let statuses = parse_compose_ps(&ps_raw);
            let services: Vec<Value> = statuses.iter().map(|(svc_name, s)| json!({
                "name": svc_name, "state": s.state,
                "running": s.state.to_lowercase().contains("running"),
                "health": s.health, "ports": s.ports,
            })).collect();
            json!({ "project": name, "path": path_str, "services": services })
        }
    }).collect();

    let projects = futures_util::future::join_all(futures).await;
    Json(json!({ "projects": projects })).into_response()
}

// ── GET /api/logs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct LogsQuery {
    pub project: String,
    pub service: Option<String>,
    pub tail: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/logs",
    operation_id = "getLogs",
    params(
        ("project" = String, Query, description = "Absolute path to the project directory"),
        ("service" = Option<String>, Query, description = "Specific service name (omit for all)"),
        ("tail" = Option<u32>, Query, description = "Number of log lines to return (default 200)"),
    ),
    responses(
        (status = 200, description = "Log output", body = LogsResponse),
        (status = 404, description = "No compose file found", body = ErrorResponse),
        (status = 500, description = "Docker error", body = ErrorResponse),
    ),
    tag = "logs"
)]
pub async fn log_fetch_handler(Query(params): Query<LogsQuery>) -> Response {
    let compose_file = match find_compose_file(&params.project) {
        Some(f) => f,
        None => return err(StatusCode::NOT_FOUND, "no compose file found"),
    };
    let cf = compose_file.to_str().unwrap().to_string();
    let tail_str = params.tail.unwrap_or(200).to_string();
    let mut args = vec!["compose", "-f", &cf, "logs", "--tail", &tail_str, "--no-color"];
    let svc_owned = params.service.clone();
    if let Some(ref s) = svc_owned { args.push(s.as_str()); }
    match run_docker(&args, None).await {
        Ok(output) => Json(LogsResponse { output }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

// ── Docker helpers ────────────────────────────────────────────────────────────

fn find_compose_file(project_path: &str) -> Option<PathBuf> {
    for name in &["docker-compose.yml", "docker-compose.yaml", "compose.yml", "compose.yaml"] {
        let full = PathBuf::from(project_path).join(name);
        if full.exists() { return Some(full); }
    }
    None
}

async fn run_docker(args: &[&str], cwd: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = Command::new("docker");
    cmd.args(args).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd { cmd.current_dir(dir); }
    let out = cmd.output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() && stdout.trim().is_empty() {
        anyhow::bail!("{}", stderr.trim());
    }
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

#[derive(Default, Clone)]
struct ServiceStatus {
    state: String,
    health: String,
    ports: Vec<String>,
}

fn parse_compose_ps(raw: &str) -> HashMap<String, ServiceStatus> {
    let mut out = HashMap::new();
    for line in raw.trim().lines() {
        let Ok(obj): Result<Value, _> = serde_json::from_str(line) else { continue };
        let name = obj["Service"].as_str().or_else(|| obj["service"].as_str()).unwrap_or("").to_string();
        if name.is_empty() { continue; }
        let ports = obj["Publishers"].as_array().unwrap_or(&vec![]).iter()
            .filter_map(|p| {
                let pub_port = p["PublishedPort"].as_u64()?;
                let target = p["TargetPort"].as_u64()?;
                if pub_port == 0 { return None; }
                Some(format!("{pub_port}:{target}"))
            })
            .collect();
        out.insert(name, ServiceStatus {
            state: obj["State"].as_str().unwrap_or("unknown").to_string(),
            health: obj["Health"].as_str().unwrap_or("").to_string(),
            ports,
        });
    }
    out
}

fn extract_library_id(text: &str) -> Option<(String, String)> {
    if let Ok(parsed) = serde_json::from_str::<Value>(text) {
        if let Some(id) = parsed["libraries"][0]["id"].as_str() {
            let title = parsed["libraries"][0]["name"].as_str().unwrap_or("").to_string();
            return Some((id.to_string(), title));
        }
    }
    let re = regex::Regex::new(r"/[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+").ok()?;
    let m = re.find(text)?;
    let id = m.as_str().to_string();
    let title = id.split('/').filter(|s| !s.is_empty()).last().unwrap_or("").to_string();
    Some((id, title))
}

// ── External spec registry ────────────────────────────────────────────────────
// Brain's own spec lives at /api/openapi.json and /api/openapi/public.json.
// These endpoints serve specs for *external* repos (rebuy and others) that are
// manually captured and stored in ~/brain/openapi/.

#[derive(Serialize, Deserialize, ToSchema, Clone)]
pub struct SpecMeta {
    pub repo: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// "manual" | "snapshot" (snapshot not yet implemented)
    pub source: String,
    #[serde(rename = "baseUrl", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(rename = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

fn specs_dir() -> std::path::PathBuf {
    crate::scanner::openapi_dir()
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
    ),
    responses(
        (status = 200, description = "Full OpenAPI spec for the repo"),
        (status = 404, description = "Spec not found", body = ErrorResponse),
        (status = 500, description = "Invalid spec JSON", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_get_handler(Path(repo): Path<String>) -> Response {
    let path = specs_dir().join(format!("{repo}.json"));
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => Json(v).into_response(),
            Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "invalid spec JSON"),
        },
        Err(_) => err(StatusCode::NOT_FOUND, &format!("no spec registered for '{repo}'")),
    }
}

#[utoipa::path(
    get,
    path = "/api/specs/{repo}/public",
    operation_id = "getSpecPublic",
    params(
        ("repo" = String, Path, description = "Repository name (e.g. admin-api)"),
    ),
    responses(
        (status = 200, description = "Public-tagged operations only"),
        (status = 404, description = "Spec not found", body = ErrorResponse),
        (status = 500, description = "Invalid spec JSON", body = ErrorResponse),
    ),
    tag = "specs"
)]
pub async fn specs_get_public_handler(Path(repo): Path<String>) -> Response {
    let path = specs_dir().join(format!("{repo}.public.json"));
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => Json(v).into_response(),
            Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "invalid spec JSON"),
        },
        Err(_) => err(StatusCode::NOT_FOUND, &format!("no public spec for '{repo}' — create {repo}.public.json in ~/brain/openapi/")),
    }
}
