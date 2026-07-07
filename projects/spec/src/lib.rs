//! `spec.*`, `spec.graphql.*`, `schema.*`, `schema.view.*` tool surfaces.
//!
//! Specs (OpenAPI/GraphQL) and schemas (DB) are first-class objects that
//! assign to a namespace via `namespace_id`. Tool bodies call directly into
//! the relevant plugins (`graphql`, `mcp`, `database`) per
//! [[feedback_no_indirection]].

mod schema;

use anyhow::{Context, Result, anyhow};
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use db::openapi_specs_registry::{
    self as registry, RegisterSpecResult, SpecMetaRow, SyncMcpSpecsResult,
};
use graphql::introspection::{
    GraphQlEnum as GqlEnum, GraphQlInfo as GqlInfo, GraphQlOperation as GqlOp,
    GraphQlType as GqlType,
};
use graphql::shopify_proxy::GraphqlProxyResult;

// ── Tool args / outputs ───────────────────────────────────────────────────

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    pub name: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SyncMcpSpecsArgs {
    pub server: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct GetSpecGraphqlInfoArgs {
    pub repo: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GraphQlInfoData {
    pub repo: String,
    pub queries: Vec<GqlOp>,
    pub mutations: Vec<GqlOp>,
    pub subscriptions: Vec<GqlOp>,
    pub types: Vec<GqlType>,
    pub inputs: Vec<GqlType>,
    pub enums: Vec<GqlEnum>,
}

// GraphQL proxy variables are arbitrary upstream JSON — opaque payload escape hatch.
#[allow(clippy::disallowed_types)]
mod proxy_graphql_args_mod {
    use super::*;
    use serde_json::Value;

    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ProxyGraphqlArgs {
        pub repo: String,
        pub shop: String,
        pub token: String,
        pub query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub variables: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub operation_name: Option<String>,
    }
}

pub use proxy_graphql_args_mod::ProxyGraphqlArgs;

fn map_info(info: GqlInfo) -> GraphQlInfoData {
    GraphQlInfoData {
        repo: info.repo,
        queries: info.queries,
        mutations: info.mutations,
        subscriptions: info.subscriptions,
        types: info.types,
        inputs: info.inputs,
        enums: info.enums,
    }
}

// ── MCP-backed sync (kept here: db cannot depend on mcp) ──────────────────
// MCP tool responses are arbitrary upstream JSON — opaque payload escape hatch.
#[allow(clippy::disallowed_types)]
mod mcp_sync {
    use super::*;
    use serde_json::{Value, json};

    fn make_mcp_pool() -> ::mcp::client::McpPool {
        // Canonical DB path (honors $ORCA_DB_PATH then $ORCA_HOME); was a
        // dirs::home_dir() fallback that ignored $ORCA_HOME.
        match contract::config::db_path() {
            Ok(path) => ::mcp::client::McpPool::new_with_db(path),
            Err(_) => ::mcp::client::McpPool::new(),
        }
    }

    pub async fn sync_mcp_specs(server: &str) -> Result<SyncMcpSpecsResult> {
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
                        cached_at: Some(utils::time::now_rfc3339()),
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
}

// ═══════════════════════════════════════════════════════════════════════════
// Tools
// ═══════════════════════════════════════════════════════════════════════════

/// List every registered OpenAPI / GraphQL spec — filesystem-resident, DB-backed, and plugin-declared — with per-source metadata.
#[orca_tool(domain = "spec", verb = "list")]
async fn list_specs(
    _args: ListSpecsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListSpecsOutput> {
    Ok(ListSpecsOutput {
        specs: registry::list_specs().await?,
    })
}

/// [MUTATES STATE] Fetch a JSON OpenAPI spec from `url` and persist it under `name` in orca.db.
#[orca_tool(domain = "spec", verb = "create")]
async fn spec_create(
    args: RegisterSpecArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    registry::register_spec(&args.name, &args.url).await
}

/// [MUTATES STATE] Re-fetch a previously-registered spec from its stored URL and update orca.db.
#[orca_tool(domain = "spec", verb = "refresh")]
async fn refresh_spec(
    args: RefreshSpecArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    registry::refresh_spec(&args.name).await
}

/// [MUTATES STATE] Remove a spec from orca.db. Returns `removed: true` when a row was deleted.
#[orca_tool(domain = "spec", verb = "delete")]
async fn spec_delete(
    args: UnregisterSpecArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<UnregisterSpecOutput> {
    Ok(UnregisterSpecOutput {
        removed: registry::unregister_spec(&args.name).await?,
    })
}

/// [MUTATES STATE] Connect to `server` (an MCP server), call its `{prefix}_spec_list` and `{prefix}_spec_schema` tools, and upsert every advertised repo into orca.db.
#[orca_tool(domain = "spec", verb = "sync-mcp")]
async fn sync_mcp_specs(
    args: SyncMcpSpecsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SyncMcpSpecsResult> {
    mcp_sync::sync_mcp_specs(&args.server).await
}

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Parse the local `<repo>.graphql` SDL into a structured types/queries/mutations view.
#[orca_tool(domain = "spec.graphql", verb = "detail")]
async fn spec_graphql_detail(
    args: GetSpecGraphqlInfoArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GraphQlInfoData> {
    if !validate_repo(&args.repo) {
        return Err(anyhow!("invalid repo name"));
    }
    let path = db::openapi_specs_registry::specs_dir().join(format!("{}.graphql", args.repo));
    let sdl = std::fs::read_to_string(&path)
        .with_context(|| format!("no GraphQL schema for '{}'", args.repo))?;
    let info = graphql::introspection::parse_graphql_sdl(&args.repo, &sdl)?;
    Ok(map_info(info))
}

/// Proxy a GraphQL request to a Shopify shop using the configured shop+token. Returns the raw upstream JSON body.
#[orca_tool(domain = "spec.graphql", verb = "update", cli = skip)]
async fn spec_graphql_update(
    args: ProxyGraphqlArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<GraphqlProxyResult> {
    graphql::shopify_proxy::proxy_graphql(
        &args.repo,
        &args.shop,
        &args.token,
        &args.query,
        args.variables,
        args.operation_name.as_deref(),
    )
    .await
}
