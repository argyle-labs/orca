//! `spec.*`, `schema.*`, `schema.view.*` tool surfaces.
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

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ListSpecsArgs {
    /// Max items to return this page (clamped to [1, 200]; default 50).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Opaque cursor from a previous page's `nextCursor`. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
    /// Opaque cursor for the next page, or absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Total rows across all pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
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
    args: ListSpecsArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ListSpecsOutput> {
    let mut specs = registry::list_specs().await?;
    specs.sort_by(|a, b| a.repo.cmp(&b.repo));
    let params = contract::paging::PageParams {
        limit: args.limit,
        cursor: args.cursor,
    };
    let page = contract::paging::Page::from_slice(specs, &params);
    Ok(ListSpecsOutput {
        specs: page.items,
        next_cursor: page.next_cursor,
        total: page.total,
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

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Parse a local `<repo>.graphql` SDL into a structured
/// types/queries/mutations view. Backs `spec.detail{format=graphql}`.
pub async fn graphql_detail(repo: &str) -> anyhow::Result<GraphQlInfoData> {
    if !validate_repo(repo) {
        return Err(anyhow!("invalid repo name"));
    }
    let path = db::openapi_specs_registry::specs_dir().join(format!("{repo}.graphql"));
    let sdl = std::fs::read_to_string(&path)
        .with_context(|| format!("no GraphQL schema for '{repo}'"))?;
    let info = graphql::introspection::parse_graphql_sdl(repo, &sdl)?;
    Ok(map_info(info))
}

// ── spec.update{format,action} ────────────────────────────────────────────

/// Which spec surface `spec.update` mutates. `openapi` (default) drives the
/// registry-backed `action`s; `graphql` runs the Shopify GraphQL proxy.
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq, Default,
)]
#[serde(rename_all = "camelCase")]
pub enum SpecFormat {
    #[default]
    Openapi,
    Graphql,
}

/// The `spec.update` action (for `format=openapi`).
#[derive(
    clap::ValueEnum, Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum SpecUpdateAction {
    /// Re-fetch a previously-registered spec from its stored URL.
    Refresh,
    /// Connect to an MCP server and upsert every advertised repo.
    SyncMcp,
}

// GraphQL proxy variables are arbitrary upstream JSON — opaque payload escape hatch.
#[allow(clippy::disallowed_types)]
mod spec_update_args_mod {
    use super::*;
    use serde_json::Value;

    #[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
    #[serde(rename_all = "camelCase", default)]
    pub struct SpecUpdateArgs {
        /// Which spec surface to mutate. Defaults to `openapi`.
        #[arg(long, value_enum, default_value = "openapi")]
        #[serde(default)]
        pub format: SpecFormat,
        /// `format=openapi`: which registry action to run (`refresh`|`sync_mcp`).
        #[arg(long, value_enum)]
        #[serde(default)]
        pub action: Option<SpecUpdateAction>,
        /// `action=refresh`: the registered spec name to re-fetch.
        #[arg(long)]
        #[serde(default)]
        pub name: Option<String>,
        /// `action=sync_mcp`: the MCP server to sync specs from.
        #[arg(long)]
        #[serde(default)]
        pub server: Option<String>,
        /// `format=graphql`: the repo whose `<repo>.graphql` schema to proxy against.
        #[arg(long)]
        #[serde(default)]
        pub repo: Option<String>,
        /// `format=graphql`: the Shopify shop domain.
        #[arg(long)]
        #[serde(default)]
        pub shop: Option<String>,
        /// `format=graphql`: the Shopify Admin API token.
        #[arg(long)]
        #[serde(default)]
        pub token: Option<String>,
        /// `format=graphql`: the GraphQL query to send.
        #[arg(long)]
        #[serde(default)]
        pub query: Option<String>,
        /// `format=graphql`: GraphQL variables (JSON object). MCP/REST only.
        #[arg(skip)]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub variables: Option<Value>,
        /// `format=graphql`: the GraphQL operation name.
        #[arg(long)]
        #[serde(default)]
        pub operation_name: Option<String>,
    }
}

pub use spec_update_args_mod::SpecUpdateArgs;

/// `spec.update` payload — one variant per format/action.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SpecUpdateOutput {
    Registry(RegisterSpecResult),
    SyncMcp(SyncMcpSpecsResult),
    Graphql(GraphqlProxyResult),
}

/// [MUTATES STATE] Update a spec surface. `format=openapi` (default) drives the
/// registry: `action=refresh` re-fetches a registered OpenAPI spec from its
/// stored URL; `action=sync_mcp` connects to an MCP server and upserts every
/// advertised repo. `format=graphql` proxies a GraphQL request to a Shopify shop
/// using the supplied shop+token and returns the raw upstream JSON body.
#[orca_tool(domain = "spec", verb = "update")]
async fn spec_update(
    args: SpecUpdateArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SpecUpdateOutput> {
    match args.format {
        SpecFormat::Graphql => {
            let repo = args
                .repo
                .ok_or_else(|| anyhow!("`repo` is required for format=graphql"))?;
            let shop = args
                .shop
                .ok_or_else(|| anyhow!("`shop` is required for format=graphql"))?;
            let token = args
                .token
                .ok_or_else(|| anyhow!("`token` is required for format=graphql"))?;
            let query = args
                .query
                .ok_or_else(|| anyhow!("`query` is required for format=graphql"))?;
            let result = graphql::shopify_proxy::proxy_graphql(
                &repo,
                &shop,
                &token,
                &query,
                args.variables,
                args.operation_name.as_deref(),
            )
            .await?;
            Ok(SpecUpdateOutput::Graphql(result))
        }
        SpecFormat::Openapi => {
            let action = args.action.ok_or_else(|| {
                anyhow!("`action` is required for format=openapi (refresh|sync_mcp)")
            })?;
            match action {
                SpecUpdateAction::Refresh => {
                    let name = args
                        .name
                        .ok_or_else(|| anyhow!("`name` is required for action=refresh"))?;
                    Ok(SpecUpdateOutput::Registry(
                        registry::refresh_spec(&name).await?,
                    ))
                }
                SpecUpdateAction::SyncMcp => {
                    let server = args
                        .server
                        .ok_or_else(|| anyhow!("`server` is required for action=sync_mcp"))?;
                    Ok(SpecUpdateOutput::SyncMcp(
                        mcp_sync::sync_mcp_specs(&server).await?,
                    ))
                }
            }
        }
    }
}
