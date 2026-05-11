//! Spec registry tools — OpenAPI + GraphQL spec discovery, registration,
//! refresh, MCP sync, and (Shopify-only) GraphQL proxy.
//!
//! Genuinely-opaque payloads:
//!   - `proxy_graphql` request `variables` and response `body` are arbitrary
//!     JSON (GQL response shapes vary per-query). Both are typed as
//!     `serde_json::Value` (the documented escape hatch).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::OrcaToolDef;

// ── Shared row shapes ───────────────────────────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecFilesPresence {
    pub full: bool,
    pub public: bool,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SyncMcpSpecsResult {
    pub server: String,
    pub synced: u32,
    pub errors: Vec<String>,
}

// ── GraphQlInfo (mirrors scanner output) ───────────────────────────────────

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<GraphQlField>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphQlEnum {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphqlProxyResult {
    pub status: u16,
    /// Raw GraphQL response body — shape varies per query, so this is
    /// intentionally arbitrary JSON. Callers downcast based on their query.
    #[cfg_attr(feature = "wasm", tsify(type = "unknown"))]
    pub body: Value,
}

// ═══════════════════════════════════════════════════════════════════════════
// Tool args/outputs
// ═══════════════════════════════════════════════════════════════════════════

// list_specs
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListSpecsOutput {
    pub specs: Vec<SpecMetaRow>,
}

pub struct ListSpecs;
impl OrcaToolDef for ListSpecs {
    const NAME: &'static str = "list_specs";
    const DESCRIPTION: &'static str = "List every registered OpenAPI / GraphQL spec — filesystem-resident, DB-backed, and \
         plugin-declared — with per-source metadata.";
    type Args = ListSpecsArgs;
    type Output = ListSpecsOutput;
}

// list_db_specs
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsArgs {}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct ListDbSpecsOutput {
    pub specs: Vec<DbSpecRow>,
}

pub struct ListDbSpecs;
impl OrcaToolDef for ListDbSpecs {
    const NAME: &'static str = "list_db_specs";
    const DESCRIPTION: &'static str =
        "List URL-registered + MCP-synced specs from orca.db (the DB-backed slice only).";
    type Args = ListDbSpecsArgs;
    type Output = ListDbSpecsOutput;
}

// register_spec
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

pub struct RegisterSpec;
impl OrcaToolDef for RegisterSpec {
    const NAME: &'static str = "register_spec";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Fetch a JSON OpenAPI spec from `url` and persist it under `name` \
         in orca.db.";
    type Args = RegisterSpecArgs;
    type Output = RegisterSpecResult;
}

// refresh_spec
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    pub name: String,
}

pub struct RefreshSpec;
impl OrcaToolDef for RefreshSpec {
    const NAME: &'static str = "refresh_spec";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Re-fetch a previously-registered spec from its stored URL and \
         update orca.db.";
    type Args = RefreshSpecArgs;
    type Output = RegisterSpecResult;
}

// unregister_spec
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct UnregisterSpecOutput {
    pub removed: bool,
}

pub struct UnregisterSpec;
impl OrcaToolDef for UnregisterSpec {
    const NAME: &'static str = "unregister_spec";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Remove a spec from orca.db. Returns `removed: true` when a row \
         was deleted.";
    type Args = UnregisterSpecArgs;
    type Output = UnregisterSpecOutput;
}

// sync_mcp_specs
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SyncMcpSpecsArgs {
    pub server: String,
}

pub struct SyncMcpSpecs;
impl OrcaToolDef for SyncMcpSpecs {
    const NAME: &'static str = "sync_mcp_specs";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Connect to `server` (an MCP server), call its `{prefix}_spec_list` \
         and `{prefix}_spec_schema` tools, and upsert every advertised repo into orca.db.";
    type Args = SyncMcpSpecsArgs;
    type Output = SyncMcpSpecsResult;
}

// get_spec_graphql_info
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
#[cfg_attr(feature = "wasm", tsify(into_wasm_abi, from_wasm_abi))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetSpecGraphqlInfoArgs {
    pub repo: String,
}

pub struct GetSpecGraphqlInfo;
impl OrcaToolDef for GetSpecGraphqlInfo {
    const NAME: &'static str = "get_spec_graphql_info";
    const DESCRIPTION: &'static str =
        "Parse the local `<repo>.graphql` SDL into a structured types/queries/mutations view.";
    type Args = GetSpecGraphqlInfoArgs;
    type Output = GraphQlInfoData;
}

// proxy_graphql
#[cfg_attr(feature = "wasm", derive(tsify_next::Tsify))]
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

pub struct ProxyGraphql;
impl OrcaToolDef for ProxyGraphql {
    const NAME: &'static str = "proxy_graphql";
    const DESCRIPTION: &'static str = "Proxy a GraphQL request to a Shopify shop using the configured shop+token. \
         Returns the raw upstream JSON body.";
    type Args = ProxyGraphqlArgs;
    type Output = GraphqlProxyResult;
}

// ═══════════════════════════════════════════════════════════════════════════
// Native run impls
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::services::spec_registry::SpecRegistryService;
    use anyhow::Result;
    use async_trait::async_trait;
    use orca_utils::tool::{OrcaTool, ToolCtx};
    use std::sync::Arc;

    fn svc(ctx: &ToolCtx) -> Result<Arc<dyn SpecRegistryService>> {
        ctx.service::<Arc<dyn SpecRegistryService>>()
    }

    #[async_trait]
    impl OrcaTool for ListSpecs {
        async fn run(_args: ListSpecsArgs, ctx: &ToolCtx) -> Result<ListSpecsOutput> {
            let specs = svc(ctx)?.list_specs().await?;
            Ok(ListSpecsOutput { specs })
        }
    }

    #[async_trait]
    impl OrcaTool for ListDbSpecs {
        async fn run(_args: ListDbSpecsArgs, ctx: &ToolCtx) -> Result<ListDbSpecsOutput> {
            let specs = svc(ctx)?.list_db_specs().await?;
            Ok(ListDbSpecsOutput { specs })
        }
    }

    #[async_trait]
    impl OrcaTool for RegisterSpec {
        async fn run(args: RegisterSpecArgs, ctx: &ToolCtx) -> Result<RegisterSpecResult> {
            svc(ctx)?.register_spec(&args.name, &args.url).await
        }
    }

    #[async_trait]
    impl OrcaTool for RefreshSpec {
        async fn run(args: RefreshSpecArgs, ctx: &ToolCtx) -> Result<RegisterSpecResult> {
            svc(ctx)?.refresh_spec(&args.name).await
        }
    }

    #[async_trait]
    impl OrcaTool for UnregisterSpec {
        async fn run(args: UnregisterSpecArgs, ctx: &ToolCtx) -> Result<UnregisterSpecOutput> {
            let removed = svc(ctx)?.unregister_spec(&args.name).await?;
            Ok(UnregisterSpecOutput { removed })
        }
    }

    #[async_trait]
    impl OrcaTool for SyncMcpSpecs {
        async fn run(args: SyncMcpSpecsArgs, ctx: &ToolCtx) -> Result<SyncMcpSpecsResult> {
            svc(ctx)?.sync_mcp_specs(&args.server).await
        }
    }

    #[async_trait]
    impl OrcaTool for GetSpecGraphqlInfo {
        async fn run(args: GetSpecGraphqlInfoArgs, ctx: &ToolCtx) -> Result<GraphQlInfoData> {
            svc(ctx)?.graphql_info(&args.repo).await
        }
    }

    #[async_trait]
    impl OrcaTool for ProxyGraphql {
        async fn run(args: ProxyGraphqlArgs, ctx: &ToolCtx) -> Result<GraphqlProxyResult> {
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
    }
}
