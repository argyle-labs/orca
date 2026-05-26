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

#[cfg(feature = "native")]
use orca_tools_macro::orca_tool;
#[cfg(feature = "native")]
use std::sync::Arc;

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
//
// GraphqlProxyResult.body is genuinely opaque — GraphQL response shapes vary
// per query and are not owned by orca. Module-level allow covers the derive
// expansion that fires disallowed_types on the Value field.

#[allow(clippy::disallowed_types)]
mod graphql_proxy_result_mod {
    use super::*;

    /// `body` is opaque — GraphQL response shapes vary per query and are not owned by orca.
    #[derive(Serialize, Deserialize, JsonSchema, Clone)]
    pub struct GraphqlProxyResult {
        pub status: u16,
        /// Raw GraphQL response body — shape varies per query, so this is
        /// intentionally arbitrary JSON. Callers downcast based on their query.
        pub body: Value,
    }
}

pub use graphql_proxy_result_mod::GraphqlProxyResult;

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

// list_specs
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
}

// list_db_specs
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsArgs {}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsOutput {
    pub specs: Vec<DbSpecRow>,
}

// register_spec
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

// refresh_spec
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    pub name: String,
}

// unregister_spec
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
}

// sync_mcp_specs
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncMcpSpecsArgs {
    pub server: String,
}

// get_spec_graphql_info
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

/// List every registered OpenAPI / GraphQL spec — filesystem-resident, DB-backed, and plugin-declared — with per-source metadata.
#[orca_tool(domain = "namespace.spec", verb = "list")]
async fn list_specs(
    _args: ListSpecsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ListSpecsOutput> {
    let specs = ctx
        .service::<Arc<dyn SpecRegistryService>>()?
        .list_specs()
        .await?;
    Ok(ListSpecsOutput { specs })
}

/// List URL-registered + MCP-synced specs from orca.db (the DB-backed slice only).
#[orca_tool(domain = "namespace.spec", verb = "list-db")]
async fn list_db_specs(
    _args: ListDbSpecsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<ListDbSpecsOutput> {
    let specs = ctx
        .service::<Arc<dyn SpecRegistryService>>()?
        .list_db_specs()
        .await?;
    Ok(ListDbSpecsOutput { specs })
}

/// [MUTATES STATE] Fetch a JSON OpenAPI spec from `url` and persist it under `name` in orca.db.
#[orca_tool(domain = "namespace.spec", verb = "create")]
async fn spec_create(
    args: RegisterSpecArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    ctx.service::<Arc<dyn SpecRegistryService>>()?
        .register_spec(&args.name, &args.url)
        .await
}

/// [MUTATES STATE] Re-fetch a previously-registered spec from its stored URL and update orca.db.
#[orca_tool(domain = "namespace.spec", verb = "refresh")]
async fn refresh_spec(
    args: RefreshSpecArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<RegisterSpecResult> {
    ctx.service::<Arc<dyn SpecRegistryService>>()?
        .refresh_spec(&args.name)
        .await
}

/// [MUTATES STATE] Remove a spec from orca.db. Returns `removed: true` when a row was deleted.
#[orca_tool(domain = "namespace.spec", verb = "delete")]
async fn spec_delete(
    args: UnregisterSpecArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<UnregisterSpecOutput> {
    let removed = ctx
        .service::<Arc<dyn SpecRegistryService>>()?
        .unregister_spec(&args.name)
        .await?;
    Ok(UnregisterSpecOutput { removed })
}

/// [MUTATES STATE] Connect to `server` (an MCP server), call its `{prefix}_spec_list` and `{prefix}_spec_schema` tools, and upsert every advertised repo into orca.db.
#[orca_tool(domain = "namespace.spec", verb = "sync-mcp")]
async fn sync_mcp_specs(
    args: SyncMcpSpecsArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<SyncMcpSpecsResult> {
    ctx.service::<Arc<dyn SpecRegistryService>>()?
        .sync_mcp_specs(&args.server)
        .await
}

/// Parse the local `<repo>.graphql` SDL into a structured types/queries/mutations view.
#[orca_tool(domain = "namespace.spec.graphql", verb = "detail")]
async fn spec_graphql_detail(
    args: GetSpecGraphqlInfoArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<GraphQlInfoData> {
    ctx.service::<Arc<dyn SpecRegistryService>>()?
        .graphql_info(&args.repo)
        .await
}

/// Proxy a GraphQL request to a Shopify shop using the configured shop+token. Returns the raw upstream JSON body.
#[orca_tool(domain = "namespace.spec.graphql", verb = "update", cli = skip)]
async fn spec_graphql_update(
    args: ProxyGraphqlArgs,
    ctx: &orca_tool::ToolCtx,
) -> anyhow::Result<GraphqlProxyResult> {
    ctx.service::<Arc<dyn SpecRegistryService>>()?
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

// ─── Service trait (impl in server crate) ────────────────────────────

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait SpecRegistryService: Send + Sync {
    /// Filesystem-rooted spec list (orca's `~/.orca/openapi/` + DB rows + plugin spec dirs).
    async fn list_specs(&self) -> Result<Vec<SpecMetaRow>>;

    /// DB-backed registry view (URL-fetched + MCP-synced specs only).
    async fn list_db_specs(&self) -> Result<Vec<DbSpecRow>>;

    /// Fetch the JSON spec at `url`, store it under `name` in orca.db.
    async fn register_spec(&self, name: &str, url: &str) -> Result<RegisterSpecResult>;

    /// Re-fetch a previously-registered spec from its stored URL.
    async fn refresh_spec(&self, name: &str) -> Result<RegisterSpecResult>;

    /// Remove a spec from the DB. Returns `true` if a row was deleted.
    async fn unregister_spec(&self, name: &str) -> Result<bool>;

    /// Connect to an MCP server and pull `{prefix}_spec_list` + `{prefix}_spec_schema`
    /// for every advertised repo.
    async fn sync_mcp_specs(&self, server: &str) -> Result<SyncMcpSpecsResult>;

    /// Parse the local `<repo>.graphql` SDL into a structured `GraphQlInfo`.
    async fn graphql_info(&self, repo: &str) -> Result<GraphQlInfoData>;

    /// Proxy a GraphQL request to a Shopify shop. Returns raw upstream JSON
    /// because GraphQL response shapes are arbitrary per-query.
    /// `variables` is opaque — GraphQL variable maps are free-form per operation.
    #[allow(clippy::disallowed_types)]
    async fn proxy_graphql(
        &self,
        repo: &str,
        shop: &str,
        token: &str,
        query: &str,
        variables: Option<serde_json::Value>,
        operation_name: Option<&str>,
    ) -> Result<GraphqlProxyResult>;
}

/// Embedder hook — see `services::mod` doc.
pub trait ProvideSpecRegistry {
    fn spec_registry(&self) -> std::sync::Arc<dyn SpecRegistryService>;
}

/// Register a `SpecRegistryService` into `ToolCtx`.
pub fn register_spec_registry(ctx: &mut orca_tool::ToolCtx, p: &impl ProvideSpecRegistry) {
    ctx.register_service(p.spec_registry());
}
