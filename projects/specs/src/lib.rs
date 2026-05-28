//! Spec registry — OpenAPI + GraphQL spec discovery, registration, refresh,
//! MCP sync, and (Shopify-only) GraphQL proxy.
//!
//! Genuinely-opaque payloads:
//!   - `proxy_graphql` request `variables` and response `body` are arbitrary
//!     JSON (GQL response shapes vary per-query). Both are typed as
//!     `serde_json::Value` (the documented escape hatch).
//!
//! Split from `docs` 2026-05-27. No service trait — tools call the free
//! functions in this crate directly per [[feedback_no_indirection]].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
// Value is used only in GraphQL proxy inner modules where all Value uses are
// legitimate opaque blobs (GQL response/variable shapes are upstream-controlled).
#[allow(clippy::disallowed_types)]
use serde_json::Value;

use orca_macro::orca_tool;

// ── Shared row shapes ───────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecFilesPresence {
    pub full: bool,
    pub public: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecMetaRow {
    pub repo: String,
    pub project: String,
    /// "manual" | "url" | "mcp" | "plugin"
    pub source: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    pub has_graphql: bool,
    pub files: SpecFilesPresence,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DbSpecRow {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSpecResult {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SyncMcpSpecsResult {
    pub server: String,
    pub synced: u32,
    pub errors: Vec<String>,
}

// ── GraphQlInfo (mirrors scanner output) ───────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GraphQlField {
    pub name: String,
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlOperation {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub args: Vec<GraphQlField>,
    pub returns: String,
    pub deprecated: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<GraphQlField>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlEnum {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlInfoData {
    pub repo: String,
    pub queries: Vec<GraphQlOperation>,
    pub mutations: Vec<GraphQlOperation>,
    pub subscriptions: Vec<GraphQlOperation>,
    pub types: Vec<GraphQlType>,
    pub inputs: Vec<GraphQlType>,
    pub enums: Vec<GraphQlEnum>,
}

// ── GraphQL proxy ──────────────────────────────────────────────────────────

#[allow(clippy::disallowed_types)]
mod graphql_proxy_result_mod {
    use super::*;

    /// `body` is opaque — GraphQL response shapes vary per query and are not owned by orca.
    #[derive(Serialize, Deserialize, JsonSchema, Clone)]
    pub struct GraphqlProxyResult {
        pub status: u16,
        pub body: Value,
    }
}

pub use graphql_proxy_result_mod::GraphqlProxyResult;

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsOutput {
    pub specs: Vec<DbSpecRow>,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    pub name: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncMcpSpecsArgs {
    pub server: String,
}

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSpecGraphqlInfoArgs {
    pub repo: String,
}

#[allow(clippy::disallowed_types)]
mod proxy_graphql_args_mod {
    use super::*;

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

mod native;

use native as imp;

// ═══════════════════════════════════════════════════════════════════════════
// Tools — call free fns in `native` directly. No service trait.
// ═══════════════════════════════════════════════════════════════════════════

/// List every registered OpenAPI / GraphQL spec — filesystem-resident, DB-backed, and plugin-declared — with per-source metadata.
#[orca_tool(domain = "namespace.spec", verb = "list")]
async fn list_specs(
    _args: ListSpecsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListSpecsOutput> {
    Ok(ListSpecsOutput {
        specs: imp::list_specs().await?,
    })
}

/// List URL-registered + MCP-synced specs from orca.db (the DB-backed slice only).
#[orca_tool(domain = "namespace.spec", verb = "list-db")]
async fn list_db_specs(
    _args: ListDbSpecsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<ListDbSpecsOutput> {
    Ok(ListDbSpecsOutput {
        specs: imp::list_db_specs().await?,
    })
}

/// [MUTATES STATE] Fetch a JSON OpenAPI spec from `url` and persist it under `name` in orca.db.
#[orca_tool(domain = "namespace.spec", verb = "create")]
async fn spec_create(
    args: RegisterSpecArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    imp::register_spec(&args.name, &args.url).await
}

/// [MUTATES STATE] Re-fetch a previously-registered spec from its stored URL and update orca.db.
#[orca_tool(domain = "namespace.spec", verb = "refresh")]
async fn refresh_spec(
    args: RefreshSpecArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    imp::refresh_spec(&args.name).await
}

/// [MUTATES STATE] Remove a spec from orca.db. Returns `removed: true` when a row was deleted.
#[orca_tool(domain = "namespace.spec", verb = "delete")]
async fn spec_delete(
    args: UnregisterSpecArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<UnregisterSpecOutput> {
    Ok(UnregisterSpecOutput {
        removed: imp::unregister_spec(&args.name).await?,
    })
}

/// [MUTATES STATE] Connect to `server` (an MCP server), call its `{prefix}_spec_list` and `{prefix}_spec_schema` tools, and upsert every advertised repo into orca.db.
#[orca_tool(domain = "namespace.spec", verb = "sync-mcp")]
async fn sync_mcp_specs(
    args: SyncMcpSpecsArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<SyncMcpSpecsResult> {
    imp::sync_mcp_specs(&args.server).await
}

/// Parse the local `<repo>.graphql` SDL into a structured types/queries/mutations view.
#[orca_tool(domain = "namespace.spec.graphql", verb = "detail")]
async fn spec_graphql_detail(
    args: GetSpecGraphqlInfoArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GraphQlInfoData> {
    imp::graphql_info(&args.repo).await
}

/// Proxy a GraphQL request to a Shopify shop using the configured shop+token. Returns the raw upstream JSON body.
#[orca_tool(domain = "namespace.spec.graphql", verb = "update", cli = skip)]
async fn spec_graphql_update(
    args: ProxyGraphqlArgs,
    _ctx: &orca_contract::ToolCtx,
) -> anyhow::Result<GraphqlProxyResult> {
    imp::proxy_graphql(
        &args.repo,
        &args.shop,
        &args.token,
        &args.query,
        args.variables,
        args.operation_name.as_deref(),
    )
    .await
}
