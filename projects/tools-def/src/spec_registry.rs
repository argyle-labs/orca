//! Spec registry tools — OpenAPI + GraphQL spec discovery, registration,
//! refresh, MCP sync, and (Shopify-only) GraphQL proxy.
//!
//! Genuinely-opaque payloads:
//!   - `proxy_graphql` request `variables` and response `body` are arbitrary
//!     JSON (GQL response shapes vary per-query). Both are typed as
//!     `serde_json::Value` (the documented escape hatch).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
// Value is used only in GraphQL proxy inner modules where all Value uses are
// legitimate opaque blobs (GQL response/variable shapes are upstream-controlled).
#[allow(clippy::disallowed_types)]
use serde_json::Value;

use crate::orca_tool;

// ── Shared row shapes ───────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecFilesPresence {
    pub full: bool,
    pub public: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SyncMcpSpecsResult {
    pub server: String,
    pub synced: u32,
    pub errors: Vec<String>,
}

// ── GraphQlInfo (mirrors scanner output) ───────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GraphQlField {
    pub name: String,
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlOperation {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub args: Vec<GraphQlField>,
    pub returns: String,
    pub deprecated: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<GraphQlField>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlEnum {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
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
//
// GraphqlProxyResult.body is genuinely opaque — GraphQL response shapes vary
// per query and are not owned by orca. Module-level allow covers the derive
// expansion that fires disallowed_types on the Value field.

#[allow(clippy::disallowed_types)]
mod graphql_proxy_result_mod {
    use super::*;

    /// `body` is opaque — GraphQL response shapes vary per query and are not owned by orca.
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema, Clone)]
    pub struct GraphqlProxyResult {
        pub status: u16,
        /// Raw GraphQL response body — shape varies per query, so this is
        /// intentionally arbitrary JSON. Callers downcast based on their query.
        #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
        pub body: Value,
    }
}

pub use graphql_proxy_result_mod::GraphqlProxyResult;

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

// list_specs
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
}

// list_db_specs
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsOutput {
    pub specs: Vec<DbSpecRow>,
}

// register_spec
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

// refresh_spec
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    pub name: String,
}

// unregister_spec
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
}

// sync_mcp_specs
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncMcpSpecsArgs {
    pub server: String,
}

// get_spec_graphql_info
#[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSpecGraphqlInfoArgs {
    pub repo: String,
}

// proxy_graphql — variables is opaque (GraphQL variable maps are free-form per operation).
#[allow(clippy::disallowed_types)]
mod proxy_graphql_args_mod {
    use super::*;

    /// `variables` is opaque — GraphQL variable maps are free-form per operation.
    #[cfg_attr(feature = "wasm", derive(tsify::Tsify))]
    #[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct ProxyGraphqlArgs {
        pub repo: String,
        /// Shopify shop domain (e.g. "myshop.myshopify.com" or "myshop").
        pub shop: String,
        /// Shopify Admin API access token.
        pub token: String,
        /// GraphQL query or mutation document.
        pub query: String,
        /// Query variables — arbitrary JSON per the GraphQL spec.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "wasm", tsify(type = "unknown | undefined"))]
        pub variables: Option<Value>,
        /// Optional operation name when the document defines multiple.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub operation_name: Option<String>,
    }
}

pub use proxy_graphql_args_mod::ProxyGraphqlArgs;

// ═══════════════════════════════════════════════════════════════════════════
// Native dispatch
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
fn svc(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::spec_registry::SpecRegistryService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::spec_registry::SpecRegistryService>>()
}

/// List every registered OpenAPI / GraphQL spec — filesystem-resident, DB-backed, and plugin-declared — with per-source metadata.
#[orca_tool(domain = "spec", verb = "list")]
async fn list_specs(
    _args: ListSpecsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListSpecsOutput> {
    let specs = svc(ctx)?.list_specs().await?;
    Ok(ListSpecsOutput { specs })
}

/// List URL-registered + MCP-synced specs from orca.db (the DB-backed slice only).
#[orca_tool(domain = "spec", verb = "list-db")]
async fn list_db_specs(
    _args: ListDbSpecsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<ListDbSpecsOutput> {
    let specs = svc(ctx)?.list_db_specs().await?;
    Ok(ListDbSpecsOutput { specs })
}

/// [MUTATES STATE] Fetch a JSON OpenAPI spec from `url` and persist it under `name` in orca.db.
#[orca_tool(domain = "spec", verb = "register")]
async fn register_spec(
    args: RegisterSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    svc(ctx)?.register_spec(&args.name, &args.url).await
}

/// [MUTATES STATE] Re-fetch a previously-registered spec from its stored URL and update orca.db.
#[orca_tool(domain = "spec", verb = "refresh")]
async fn refresh_spec(
    args: RefreshSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    svc(ctx)?.refresh_spec(&args.name).await
}

/// [MUTATES STATE] Remove a spec from orca.db. Returns `removed: true` when a row was deleted.
#[orca_tool(domain = "spec", verb = "unregister")]
async fn unregister_spec(
    args: UnregisterSpecArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<UnregisterSpecOutput> {
    let removed = svc(ctx)?.unregister_spec(&args.name).await?;
    Ok(UnregisterSpecOutput { removed })
}

/// [MUTATES STATE] Connect to `server` (an MCP server), call its `{prefix}_spec_list` and `{prefix}_spec_schema` tools, and upsert every advertised repo into orca.db.
#[orca_tool(domain = "spec", verb = "sync-mcp")]
async fn sync_mcp_specs(
    args: SyncMcpSpecsArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SyncMcpSpecsResult> {
    svc(ctx)?.sync_mcp_specs(&args.server).await
}

/// Parse the local `<repo>.graphql` SDL into a structured types/queries/mutations view.
#[orca_tool(domain = "spec", verb = "graphql-info")]
async fn get_spec_graphql_info(
    args: GetSpecGraphqlInfoArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GraphQlInfoData> {
    svc(ctx)?.graphql_info(&args.repo).await
}

/// Proxy a GraphQL request to a Shopify shop using the configured shop+token. Returns the raw upstream JSON body.
#[orca_tool(domain = "spec", verb = "proxy-graphql", cli = skip)]
async fn proxy_graphql(
    args: ProxyGraphqlArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GraphqlProxyResult> {
    svc(ctx)?
        .proxy_graphql(
            &args.repo,
            &args.shop,
            &args.token,
            &args.query,
            args.variables,
            args.operation_name.as_deref(),
        )
        .await
}
