use anyhow::Result;
use async_trait::async_trait;
use orca_utils::tool::{OrcaTool, OrcaToolDef, ToolCtx};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::mcp::specs;

// ── list_rebuy_specs ──────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ListRebuySpecsArgs {}

pub struct ListRebuySpecs;

impl OrcaToolDef for ListRebuySpecs {
    const NAME: &'static str = "list_rebuy_specs";
    const DESCRIPTION: &'static str = "List all registered OpenAPI specs for rebuy repos. Returns repo name, description, \
         path count, and whether a public or GraphQL schema is available.";
    type Args = ListRebuySpecsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for ListRebuySpecs {
    async fn run(_args: ListRebuySpecsArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::list_rebuy_specs()
    }
}
// ── get_rebuy_spec ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetRebuySpecArgs {
    /// Repo name (e.g. admin-api, apiv2, rebuyengine)
    pub repo: String,
}

pub struct GetRebuySpec;

impl OrcaToolDef for GetRebuySpec {
    const NAME: &'static str = "get_rebuy_spec";
    const DESCRIPTION: &'static str = "Read the full OpenAPI spec for a rebuy repo (e.g. admin-api, apiv2). \
         Returns the complete JSON spec.";
    type Args = GetRebuySpecArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetRebuySpec {
    async fn run(args: GetRebuySpecArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::get_rebuy_spec(&json!({ "repo": args.repo }))
    }
}
// ── get_rebuy_spec_public ─────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetRebuySpecPublicArgs {
    /// Repo name (e.g. admin-api, apiv2)
    pub repo: String,
}

pub struct GetRebuySpecPublic;

impl OrcaToolDef for GetRebuySpecPublic {
    const NAME: &'static str = "get_rebuy_spec_public";
    const DESCRIPTION: &'static str = "Read the public-only OpenAPI spec for a rebuy repo. Contains only publicly \
         documented endpoints.";
    type Args = GetRebuySpecPublicArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetRebuySpecPublic {
    async fn run(args: GetRebuySpecPublicArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::get_rebuy_spec_public(&json!({ "repo": args.repo }))
    }
}
// ── get_rebuy_graphql_schema ──────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetRebuyGraphqlSchemaArgs {
    /// Repo name (e.g. admin-api)
    pub repo: String,
}

pub struct GetRebuyGraphqlSchema;

impl OrcaToolDef for GetRebuyGraphqlSchema {
    const NAME: &'static str = "get_rebuy_graphql_schema";
    const DESCRIPTION: &'static str =
        "Read the raw GraphQL SDL schema for a rebuy repo. Returns the full SDL text.";
    type Args = GetRebuyGraphqlSchemaArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetRebuyGraphqlSchema {
    async fn run(args: GetRebuyGraphqlSchemaArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::get_rebuy_graphql_schema(&json!({ "repo": args.repo }))
    }
}
// ── get_graphql_info ──────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct GetGraphqlInfoArgs {
    /// Repo name (e.g. admin-api)
    pub repo: String,
}

pub struct GetGraphqlInfo;

impl OrcaToolDef for GetGraphqlInfo {
    const NAME: &'static str = "get_graphql_info";
    const DESCRIPTION: &'static str = "Parse and return structured GraphQL schema info for a rebuy repo: queries, mutations, \
         subscriptions, types, inputs, and enums — each with field names, types, and descriptions. \
         Use this instead of get_rebuy_graphql_schema when you need to reason about the schema \
         rather than read the raw SDL.";
    type Args = GetGraphqlInfoArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetGraphqlInfo {
    async fn run(args: GetGraphqlInfoArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::get_graphql_info(&json!({ "repo": args.repo }))
    }
}
// ── register_spec ─────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct RegisterSpecArgs {
    pub name: String,
    pub url: String,
}

pub struct RegisterSpec;

impl OrcaToolDef for RegisterSpec {
    const NAME: &'static str = "register_spec";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Fetch an OpenAPI spec from a URL and register it in the orca DB. \
         Use refresh_spec to re-fetch after updates.";
    type Args = RegisterSpecArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for RegisterSpec {
    async fn run(args: RegisterSpecArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::spec_register(&json!({ "name": args.name, "url": args.url })).await
    }
}
// ── refresh_spec ──────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct RefreshSpecArgs {
    /// Refresh all URL-registered specs
    pub all: Option<bool>,
    /// Refresh a specific spec by name
    pub name: Option<String>,
}

pub struct RefreshSpec;

impl OrcaToolDef for RefreshSpec {
    const NAME: &'static str = "refresh_spec";
    const DESCRIPTION: &'static str = "[MUTATES STATE] Re-fetch and update one or all URL-registered OpenAPI specs. \
         Provide name or set all=true.";
    type Args = RefreshSpecArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for RefreshSpec {
    async fn run(args: RefreshSpecArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::spec_refresh(&json!({ "all": args.all, "name": args.name })).await
    }
}
// ── unregister_spec ───────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct UnregisterSpecArgs {
    pub name: String,
}

pub struct UnregisterSpec;

impl OrcaToolDef for UnregisterSpec {
    const NAME: &'static str = "unregister_spec";
    const DESCRIPTION: &'static str =
        "[MUTATES STATE] Remove a URL-registered OpenAPI spec from the orca DB by name.";
    type Args = UnregisterSpecArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for UnregisterSpec {
    async fn run(args: UnregisterSpecArgs, _ctx: &ToolCtx) -> Result<String> {
        use serde_json::json;
        specs::spec_unregister(&json!({ "name": args.name }))
    }
}
// ── register ──────────────────────────────────────────────────────────────────

pub fn register(reg: &mut orca_utils::tool::ToolRegistry) {
    reg.register::<ListRebuySpecs>()
        .register::<GetRebuySpec>()
        .register::<GetRebuySpecPublic>()
        .register::<GetRebuyGraphqlSchema>()
        .register::<GetGraphqlInfo>()
        .register::<RegisterSpec>()
        .register::<RefreshSpec>()
        .register::<UnregisterSpec>();
}
