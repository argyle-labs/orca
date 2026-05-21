//! Server-side impl of `SpecRegistryService` — mirrors the HTTP handlers in
//! `serve::api::specs` but typed all the way through.
#![allow(clippy::disallowed_types)] // OpenAPI spec registry — dynamic JSON construction for spec blobs

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use orca_tools_def::services::spec_registry::SpecRegistryService;
use orca_tools_def::spec_registry::{
    DbSpecRow, GraphQlEnum, GraphQlField, GraphQlInfoData, GraphQlOperation, GraphQlType,
    GraphqlProxyResult, RegisterSpecResult, SpecFilesPresence, SpecMetaRow, SyncMcpSpecsResult,
};
use serde_json::{Value, json};

use crate::scanner::specs_dir;
use crate::scanner::{
    GraphQlEnum as ScannerEnum, GraphQlField as ScannerField, GraphQlInfo as ScannerInfo,
    GraphQlOperation as ScannerOp, GraphQlType as ScannerType,
};

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub struct ServerSpecRegistry;

fn map_field(f: ScannerField) -> GraphQlField {
    GraphQlField {
        name: f.name,
        type_name: f.type_name,
        description: f.description,
        required: f.required,
    }
}

fn map_op(op: ScannerOp) -> GraphQlOperation {
    GraphQlOperation {
        name: op.name,
        description: op.description,
        args: op.args.into_iter().map(map_field).collect(),
        returns: op.returns,
        deprecated: op.deprecated,
    }
}

fn map_type(t: ScannerType) -> GraphQlType {
    GraphQlType {
        name: t.name,
        description: t.description,
        fields: t.fields.into_iter().map(map_field).collect(),
    }
}

fn map_enum(e: ScannerEnum) -> GraphQlEnum {
    GraphQlEnum {
        name: e.name,
        description: e.description,
        values: e.values,
    }
}

fn map_info(info: ScannerInfo) -> GraphQlInfoData {
    GraphQlInfoData {
        repo: info.repo,
        queries: info.queries.into_iter().map(map_op).collect(),
        mutations: info.mutations.into_iter().map(map_op).collect(),
        subscriptions: info.subscriptions.into_iter().map(map_op).collect(),
        types: info.types.into_iter().map(map_type).collect(),
        inputs: info.inputs.into_iter().map(map_type).collect(),
        enums: info.enums.into_iter().map(map_enum).collect(),
    }
}

fn make_mcp_pool() -> crate::serve::mcp_client::McpPool {
    use orca_utils::config::{APP_DB_FILE, APP_STATE_DIR};
    if let Ok(path) = std::env::var("ORCA_DB_PATH") {
        return crate::serve::mcp_client::McpPool::new_with_db(std::path::PathBuf::from(path));
    }
    if let Some(home) = dirs::home_dir() {
        return crate::serve::mcp_client::McpPool::new_with_db(
            home.join(APP_STATE_DIR).join(APP_DB_FILE),
        );
    }
    crate::serve::mcp_client::McpPool::new()
}

#[async_trait]
impl SpecRegistryService for ServerSpecRegistry {
    async fn list_specs(&self) -> Result<Vec<SpecMetaRow>> {
        let dir = specs_dir();

        // Optional registry.json provides extra metadata (project, description, baseUrl).
        let registry: Vec<Value> = match std::fs::read_to_string(dir.join("registry.json")) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut by_repo: std::collections::HashMap<String, Value> = registry
            .into_iter()
            .filter_map(|e| {
                let repo = e.get("repo")?.as_str()?.to_string();
                Some((repo, e))
            })
            .collect();

        let mut out: Vec<SpecMetaRow> = Vec::new();

        if let Ok(read) = std::fs::read_dir(&dir) {
            let mut repos: Vec<String> = read
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name == "registry.json" {
                        return None;
                    }
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

            for repo in repos {
                let entry = by_repo.remove(&repo);
                let project = entry
                    .as_ref()
                    .and_then(|v| v.get("project"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| repo.clone());
                let base_url = entry
                    .as_ref()
                    .and_then(|v| v.get("baseUrl"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let source = entry
                    .as_ref()
                    .and_then(|v| v.get("source"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("manual")
                    .to_string();
                let has_full = dir.join(format!("{repo}.json")).exists();
                let has_public = dir.join(format!("{repo}.public.json")).exists();
                let has_graphql = dir.join(format!("{repo}.graphql")).exists();
                out.push(SpecMetaRow {
                    repo,
                    project,
                    source,
                    namespace: "orca".to_string(),
                    source_mcp: None,
                    base_url,
                    captured_at: None,
                    path_count: None,
                    has_graphql,
                    files: SpecFilesPresence {
                        full: has_full,
                        public: has_public,
                    },
                });
            }
        }

        if let Ok(conn) = db::open_default() {
            if let Ok(db_specs) = db::openapi_specs::list(&conn) {
                let disk_names: std::collections::HashSet<String> =
                    out.iter().map(|r| r.repo.clone()).collect();
                for s in db_specs {
                    if disk_names.contains(&s.name) {
                        continue;
                    }
                    let path_count = s
                        .spec_json
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .and_then(|v| v["paths"].as_object().map(|p| p.len() as u32));
                    let namespace = s.source_mcp.clone().unwrap_or_else(|| "orca".to_string());
                    let source = if s.source_mcp.is_some() { "mcp" } else { "url" };
                    out.push(SpecMetaRow {
                        repo: s.name.clone(),
                        project: s.name,
                        source: source.to_string(),
                        namespace,
                        source_mcp: s.source_mcp,
                        base_url: s.url,
                        captured_at: s.cached_at,
                        path_count,
                        has_graphql: false,
                        files: SpecFilesPresence {
                            full: true,
                            public: false,
                        },
                    });
                }
            }

            if let Ok(plugins) = db::plugins::list(&conn) {
                for plugin in plugins
                    .iter()
                    .filter(|p| p.specs_dir.is_some() && p.enabled)
                {
                    let plugin_dir = std::path::PathBuf::from(plugin.specs_dir.as_deref().unwrap());
                    let Ok(read) = std::fs::read_dir(&plugin_dir) else {
                        continue;
                    };
                    let mut seen: std::collections::HashSet<String> =
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
                        if !seen.insert(repo.clone()) {
                            continue;
                        }
                        let has_full = plugin_dir.join(format!("{repo}.json")).exists();
                        let has_public = plugin_dir.join(format!("{repo}.public.json")).exists();
                        let has_graphql = plugin_dir.join(format!("{repo}.graphql")).exists();
                        out.push(SpecMetaRow {
                            repo: repo.clone(),
                            project: repo,
                            source: "plugin".to_string(),
                            namespace: plugin.id.clone(),
                            source_mcp: None,
                            base_url: None,
                            captured_at: None,
                            path_count: None,
                            has_graphql,
                            files: SpecFilesPresence {
                                full: has_full,
                                public: has_public,
                            },
                        });
                    }
                }
            }
        }

        Ok(out)
    }

    async fn list_db_specs(&self) -> Result<Vec<DbSpecRow>> {
        let conn = db::open_default()?;
        let rows = db::openapi_specs::list(&conn)?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let path_count = r
                    .spec_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|v| v["paths"].as_object().map(|p| p.len() as u32));
                DbSpecRow {
                    name: r.name,
                    url: r.url,
                    source_mcp: r.source_mcp,
                    path_count,
                    cached_at: r.cached_at,
                    enabled: r.enabled,
                }
            })
            .collect())
    }

    async fn register_spec(&self, name: &str, url: &str) -> Result<RegisterSpecResult> {
        if name.is_empty() || url.is_empty() {
            return Err(anyhow!("name and url are required"));
        }
        let resp = reqwest::get(url)
            .await
            .with_context(|| format!("fetch {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} fetching {url}", resp.status()));
        }
        let spec_json: Value = resp.json().await.context("invalid JSON from spec URL")?;
        let spec_text = serde_json::to_string(&spec_json)?;
        let path_count = spec_json["paths"].as_object().map(|p| p.len() as u32);
        let cached_at = chrono::Utc::now().to_rfc3339();
        let conn = db::open_default()?;
        let row = db::openapi_specs::OpenApiSpecRow {
            name: name.to_string(),
            url: Some(url.to_string()),
            source_mcp: None,
            spec_json: Some(spec_text),
            cached_at: Some(cached_at.clone()),
            enabled: true,
        };
        db::openapi_specs::upsert(&conn, &row)?;
        Ok(RegisterSpecResult {
            name: name.to_string(),
            url: Some(url.to_string()),
            source_mcp: None,
            path_count,
            cached_at: Some(cached_at),
            enabled: true,
        })
    }

    async fn refresh_spec(&self, name: &str) -> Result<RegisterSpecResult> {
        if !validate_repo(name) {
            return Err(anyhow!("invalid spec name"));
        }
        let conn = db::open_default()?;
        let row = db::openapi_specs::get(&conn, name)?
            .ok_or_else(|| anyhow!("no spec named '{name}'"))?;
        let url = row
            .url
            .clone()
            .ok_or_else(|| anyhow!("spec '{name}' has no URL — cannot refresh"))?;
        let resp = reqwest::get(&url)
            .await
            .with_context(|| format!("fetch {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} fetching {url}", resp.status()));
        }
        let spec_json: Value = resp.json().await.context("invalid JSON from spec URL")?;
        let spec_text = serde_json::to_string(&spec_json)?;
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
        db::openapi_specs::upsert(&conn, &updated)?;
        Ok(RegisterSpecResult {
            name: row.name,
            url: row.url,
            source_mcp: row.source_mcp,
            path_count,
            cached_at: Some(cached_at),
            enabled: row.enabled,
        })
    }

    async fn unregister_spec(&self, name: &str) -> Result<bool> {
        if !validate_repo(name) {
            return Err(anyhow!("invalid spec name"));
        }
        let conn = db::open_default()?;
        Ok(db::openapi_specs::remove(&conn, name)?)
    }

    async fn sync_mcp_specs(&self, server: &str) -> Result<SyncMcpSpecsResult> {
        let pool = make_mcp_pool();
        let prefix = server.split('-').next().unwrap_or(server).to_string();
        let list_tool = format!("{prefix}_spec_list");
        let client = pool
            .get_or_connect(server)
            .await
            .with_context(|| format!("connect MCP server '{server}'"))?;

        let list_result = client
            .call_tool(&list_tool, json!({}), "sync-mcp")
            .await
            .with_context(|| format!("{list_tool} failed"))?;

        let text = list_result["content"]
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find_map(|c| c["text"].as_str().map(str::to_string))
            })
            .unwrap_or_default();

        let repos: Vec<String> = if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&text) {
            arr.into_iter()
                .filter_map(|v| {
                    v["repo"]
                        .as_str()
                        .or_else(|| v["name"].as_str())
                        .or_else(|| v.as_str())
                        .map(str::to_string)
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
            return Err(anyhow!("MCP spec list returned no repos"));
        }

        let schema_tool = format!("{prefix}_spec_schema");
        let conn = db::open_default()?;
        let mut synced = 0u32;
        let mut errors: Vec<String> = Vec::new();

        for repo in &repos {
            if repo.is_empty() {
                continue;
            }
            match client
                .call_tool(&schema_tool, json!({ "repo": repo }), "sync-mcp")
                .await
            {
                Err(e) => errors.push(format!("{repo}: {e}")),
                Ok(r) => {
                    let spec_text = r["content"].as_array().and_then(|arr| {
                        arr.iter()
                            .find_map(|c| c["text"].as_str().map(str::to_string))
                    });
                    let Some(spec_text) = spec_text else {
                        errors.push(format!("{repo}: empty schema response"));
                        continue;
                    };
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

        Ok(SyncMcpSpecsResult {
            server: server.to_string(),
            synced,
            errors,
        })
    }

    async fn graphql_info(&self, repo: &str) -> Result<GraphQlInfoData> {
        if !validate_repo(repo) {
            return Err(anyhow!("invalid repo name"));
        }
        let path = specs_dir().join(format!("{repo}.graphql"));
        let sdl = std::fs::read_to_string(&path)
            .with_context(|| format!("no GraphQL schema for '{repo}'"))?;
        let info = crate::scanner::parse_graphql_sdl(repo, &sdl)?;
        Ok(map_info(info))
    }

    async fn proxy_graphql(
        &self,
        repo: &str,
        shop: &str,
        token: &str,
        query: &str,
        variables: Option<Value>,
        operation_name: Option<&str>,
    ) -> Result<GraphqlProxyResult> {
        if !validate_repo(repo) {
            return Err(anyhow!("invalid repo name"));
        }
        let version = shopify_admin_version();
        let trimmed = shop.trim().trim_end_matches('/');
        let shop_domain = if trimmed.contains('.') {
            trimmed.to_string()
        } else {
            format!("{trimmed}.myshopify.com")
        };
        let url = format!("https://{shop_domain}/admin/api/{version}/graphql.json");

        let mut payload = json!({ "query": query });
        if let Some(vars) = variables {
            payload["variables"] = vars;
        }
        if let Some(op) = operation_name {
            payload["operationName"] = Value::String(op.to_string());
        }

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-Shopify-Access-Token", token)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        let status = resp.status().as_u16();
        let body: Value = resp
            .json()
            .await
            .context("upstream did not return JSON")
            .unwrap_or(Value::Null);
        Ok(GraphqlProxyResult { status, body })
    }
}

fn shopify_admin_version() -> String {
    use serde::Deserialize;
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
