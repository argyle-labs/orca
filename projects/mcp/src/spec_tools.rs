//! OpenAPI / GraphQL spec-registry tools — server-local because their
//! backing implementations live in `crate::specs` (file-system layout
//! of the operator's checkout).
//!
//! These tools predate the `#[orca_tool]` macro. They expose
//! generic spec lookup over an MCP surface and ship hand-rolled
//! `OrcaTool` impls + inventory entries; everything else
//! (MCP/REST/OpenAPI emission) flows through the standard
//! `orca_dispatch` paths.

use anyhow::Result;
use async_trait::async_trait;
use orca_contract::{OrcaTool, OrcaToolDef, ToolCtx};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::specs;

// ── list_specs ──────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ListSpecsArgs {}

pub struct ListSpecs;

impl OrcaToolDef for ListSpecs {
    const NAME: &'static str = "list_specs";
    const DESCRIPTION: &'static str = "List all registered OpenAPI specs for registered repos. Returns repo name, description, \
         path count, and whether a public or GraphQL schema is available.";
    type Args = ListSpecsArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for ListSpecs {
    async fn run(_args: ListSpecsArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::list_specs()
    }
}

// ── get_spec ────────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GetSpecArgs {
    /// Registered repo name
    pub repo: String,
}

pub struct GetSpec;

impl OrcaToolDef for GetSpec {
    const NAME: &'static str = "get_spec";
    const DESCRIPTION: &'static str = "Read the full OpenAPI spec for a registered repo (e.g. admin-api, apiv2). \
         Returns the complete JSON spec.";
    type Args = GetSpecArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetSpec {
    async fn run(args: GetSpecArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::get_spec(&json!({ "repo": args.repo }))
    }
}

// ── get_spec_public ─────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GetSpecPublicArgs {
    /// Repo name (e.g. admin-api, apiv2)
    pub repo: String,
}

pub struct GetSpecPublic;

impl OrcaToolDef for GetSpecPublic {
    const NAME: &'static str = "get_spec_public";
    const DESCRIPTION: &'static str = "Read the public-only OpenAPI spec for a registered repo. Contains only publicly \
         documented endpoints.";
    type Args = GetSpecPublicArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetSpecPublic {
    async fn run(args: GetSpecPublicArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::get_spec_public(&json!({ "repo": args.repo }))
    }
}

// ── get_graphql_schema ──────────────────────────────────────────────────

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GetGraphqlSchemaArgs {
    /// Repo name (e.g. admin-api)
    pub repo: String,
}

pub struct GetGraphqlSchema;

impl OrcaToolDef for GetGraphqlSchema {
    const NAME: &'static str = "get_graphql_schema";
    const DESCRIPTION: &'static str =
        "Read the raw GraphQL SDL schema for a registered repo. Returns the full SDL text.";
    type Args = GetGraphqlSchemaArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetGraphqlSchema {
    async fn run(args: GetGraphqlSchemaArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::get_graphql_schema(&json!({ "repo": args.repo }))
    }
}

// ── get_graphql_info ──────────────────────────────────────────────────────────

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct GetGraphqlInfoArgs {
    /// Repo name (e.g. admin-api)
    pub repo: String,
}

pub struct GetGraphqlInfo;

impl OrcaToolDef for GetGraphqlInfo {
    const NAME: &'static str = "get_graphql_info";
    const DESCRIPTION: &'static str = "Parse and return structured GraphQL schema info for a registered repo: queries, mutations, \
         subscriptions, types, inputs, and enums — each with field names, types, and descriptions. \
         Use this instead of get_graphql_schema when you need to reason about the schema \
         rather than read the raw SDL.";
    type Args = GetGraphqlInfoArgs;
    type Output = String;
}

#[async_trait]
impl OrcaTool for GetGraphqlInfo {
    async fn run(args: GetGraphqlInfoArgs, _ctx: &ToolCtx) -> Result<String> {
        specs::get_graphql_info(&json!({ "repo": args.repo }))
    }
}

// ── inventory registration ────────────────────────────────────────────────────
//
// These five tools have hand-picked NAMEs that don't fit `{domain}.{verb}`, so
// they can't go through the `#[orca_tool]` macro. We hand-submit their
// `ToolRegistration` entries — same slice the macro fills for every other tool.

macro_rules! register_spec_tool {
    ($tool:ty) => {
        ::inventory::submit! {
            ::orca_dispatch::ToolRegistration {
                name: <$tool as ::orca_contract::OrcaToolDef>::NAME,
                make_erased: || ::std::boxed::Box::new(
                    ::orca_dispatch::ToolWrapper::<$tool>(::std::marker::PhantomData)
                ),
            }
        }
    };
}

register_spec_tool!(ListSpecs);
register_spec_tool!(GetSpec);
register_spec_tool!(GetSpecPublic);
register_spec_tool!(GetGraphqlSchema);
register_spec_tool!(GetGraphqlInfo);
