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

#[cfg(test)]
mod tests {
    use super::*;
    use graphql::introspection::{GraphQlField, GraphQlOperation};

    // ── validate_repo ──────────────────────────────────────────────────────

    #[test]
    fn validate_repo_accepts_typical_names() {
        assert!(validate_repo("shopify"));
        assert!(validate_repo("my-repo"));
        assert!(validate_repo("my_repo"));
        assert!(validate_repo("repo.v2"));
        assert!(validate_repo("Repo123"));
    }

    #[test]
    fn validate_repo_rejects_empty() {
        assert!(!validate_repo(""));
    }

    #[test]
    fn validate_repo_rejects_path_traversal_and_separators() {
        assert!(!validate_repo("../etc/passwd"));
        assert!(!validate_repo("a/b"));
        assert!(!validate_repo("a b"));
        assert!(!validate_repo("a:b"));
        assert!(!validate_repo("a$b"));
    }

    // ── map_info ───────────────────────────────────────────────────────────

    fn sample_op(name: &str) -> GqlOp {
        GraphQlOperation {
            name: name.to_string(),
            description: None,
            args: vec![GraphQlField {
                name: "id".to_string(),
                type_name: "ID".to_string(),
                description: None,
                required: true,
            }],
            returns: "String".to_string(),
            deprecated: false,
        }
    }

    fn sample_type(name: &str) -> GqlType {
        GqlType {
            name: name.to_string(),
            description: None,
            fields: vec![],
        }
    }

    fn sample_enum(name: &str) -> GqlEnum {
        GqlEnum {
            name: name.to_string(),
            description: None,
            values: vec!["A".to_string(), "B".to_string()],
        }
    }

    #[test]
    fn map_info_preserves_all_fields() {
        let info = GqlInfo {
            repo: "shopify".to_string(),
            queries: vec![sample_op("q1")],
            mutations: vec![sample_op("m1"), sample_op("m2")],
            subscriptions: vec![sample_op("s1")],
            types: vec![sample_type("T1")],
            inputs: vec![sample_type("I1")],
            enums: vec![sample_enum("E1")],
        };
        let out = map_info(info);
        assert_eq!(out.repo, "shopify");
        assert_eq!(out.queries.len(), 1);
        assert_eq!(out.mutations.len(), 2);
        assert_eq!(out.subscriptions.len(), 1);
        assert_eq!(out.types.len(), 1);
        assert_eq!(out.inputs.len(), 1);
        assert_eq!(out.enums.len(), 1);
        assert_eq!(out.queries[0].name, "q1");
        assert_eq!(out.enums[0].values, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn map_info_handles_empty() {
        let info = GqlInfo {
            repo: "empty".to_string(),
            queries: vec![],
            mutations: vec![],
            subscriptions: vec![],
            types: vec![],
            inputs: vec![],
            enums: vec![],
        };
        let out = map_info(info);
        assert_eq!(out.repo, "empty");
        assert!(out.queries.is_empty());
        assert!(out.mutations.is_empty());
    }

    // ── ListSpecsArgs / ListSpecsOutput serde ──────────────────────────────

    #[test]
    fn list_specs_args_default_is_empty() {
        let args = ListSpecsArgs::default();
        assert!(args.limit.is_none());
        assert!(args.cursor.is_none());
    }

    #[test]
    fn list_specs_args_deserializes_camel_case() {
        let args: ListSpecsArgs =
            serde_json::from_str(r#"{"limit":25,"cursor":"abc"}"#).expect("parse");
        assert_eq!(args.limit, Some(25));
        assert_eq!(args.cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn list_specs_args_deserializes_empty_object() {
        let args: ListSpecsArgs = serde_json::from_str("{}").expect("parse");
        assert!(args.limit.is_none());
        assert!(args.cursor.is_none());
    }

    #[test]
    fn list_specs_output_omits_absent_cursor_and_total() {
        let out = ListSpecsOutput {
            specs: vec![],
            next_cursor: None,
            total: None,
        };
        let s = serde_json::to_string(&out).expect("serialize");
        assert_eq!(s, r#"{"specs":[]}"#);
    }

    #[test]
    fn list_specs_output_includes_present_cursor_and_total() {
        let out = ListSpecsOutput {
            specs: vec![],
            next_cursor: Some("next".to_string()),
            total: Some(7),
        };
        let s = serde_json::to_string(&out).expect("serialize");
        assert!(s.contains(r#""next_cursor":"next""#));
        assert!(s.contains(r#""total":7"#));
    }

    // ── RegisterSpecArgs / UnregisterSpecArgs / Output ─────────────────────

    #[test]
    fn register_spec_args_roundtrip() {
        let args: RegisterSpecArgs =
            serde_json::from_str(r#"{"name":"foo","url":"https://x/y.json"}"#).expect("parse");
        assert_eq!(args.name, "foo");
        assert_eq!(args.url, "https://x/y.json");
        let s = serde_json::to_string(&args).expect("serialize");
        assert!(s.contains(r#""name":"foo""#));
        assert!(s.contains(r#""url":"https://x/y.json""#));
    }

    #[test]
    fn unregister_spec_args_parse() {
        let args: UnregisterSpecArgs = serde_json::from_str(r#"{"name":"bar"}"#).expect("parse");
        assert_eq!(args.name, "bar");
    }

    #[test]
    fn unregister_spec_output_serializes_bool() {
        let s = serde_json::to_string(&UnregisterSpecOutput { removed: true }).expect("serialize");
        assert_eq!(s, r#"{"removed":true}"#);
        let s = serde_json::to_string(&UnregisterSpecOutput { removed: false }).expect("serialize");
        assert_eq!(s, r#"{"removed":false}"#);
    }

    // ── SpecFormat / SpecUpdateAction enums ────────────────────────────────

    #[test]
    fn spec_format_defaults_to_openapi() {
        assert_eq!(SpecFormat::default(), SpecFormat::Openapi);
    }

    #[test]
    fn spec_format_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&SpecFormat::Openapi).expect("serialize"),
            r#""openapi""#
        );
        assert_eq!(
            serde_json::to_string(&SpecFormat::Graphql).expect("serialize"),
            r#""graphql""#
        );
    }

    #[test]
    fn spec_format_deserializes_camel_case() {
        let f: SpecFormat = serde_json::from_str(r#""graphql""#).expect("parse");
        assert_eq!(f, SpecFormat::Graphql);
    }

    #[test]
    fn spec_update_action_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SpecUpdateAction::Refresh).expect("serialize"),
            r#""refresh""#
        );
        assert_eq!(
            serde_json::to_string(&SpecUpdateAction::SyncMcp).expect("serialize"),
            r#""sync_mcp""#
        );
    }

    #[test]
    fn spec_update_action_deserializes_snake_case() {
        let a: SpecUpdateAction = serde_json::from_str(r#""sync_mcp""#).expect("parse");
        assert_eq!(a, SpecUpdateAction::SyncMcp);
    }

    // ── SpecUpdateArgs serde ───────────────────────────────────────────────

    #[test]
    fn spec_update_args_default_is_openapi_with_no_action() {
        let args = SpecUpdateArgs::default();
        assert_eq!(args.format, SpecFormat::Openapi);
        assert!(args.action.is_none());
        assert!(args.name.is_none());
        assert!(args.server.is_none());
        assert!(args.repo.is_none());
    }

    #[test]
    fn spec_update_args_empty_object_uses_defaults() {
        let args: SpecUpdateArgs = serde_json::from_str("{}").expect("parse");
        assert_eq!(args.format, SpecFormat::Openapi);
        assert!(args.action.is_none());
    }

    #[test]
    fn spec_update_args_parses_refresh() {
        let args: SpecUpdateArgs =
            serde_json::from_str(r#"{"action":"refresh","name":"pets"}"#).expect("parse");
        assert_eq!(args.format, SpecFormat::Openapi);
        assert_eq!(args.action, Some(SpecUpdateAction::Refresh));
        assert_eq!(args.name.as_deref(), Some("pets"));
    }

    #[test]
    fn spec_update_args_parses_graphql_fields() {
        let args: SpecUpdateArgs = serde_json::from_str(
            r#"{"format":"graphql","repo":"shopify","shop":"s.myshopify.com","token":"t","query":"{shop{name}}","operationName":"Op"}"#,
        )
        .expect("parse");
        assert_eq!(args.format, SpecFormat::Graphql);
        assert_eq!(args.repo.as_deref(), Some("shopify"));
        assert_eq!(args.shop.as_deref(), Some("s.myshopify.com"));
        assert_eq!(args.token.as_deref(), Some("t"));
        assert_eq!(args.query.as_deref(), Some("{shop{name}}"));
        assert_eq!(args.operation_name.as_deref(), Some("Op"));
    }

    // ── SpecUpdateOutput untagged serialization ────────────────────────────

    #[test]
    fn spec_update_output_registry_variant_is_untagged() {
        let out = SpecUpdateOutput::Registry(RegisterSpecResult {
            name: "pets".to_string(),
            url: Some("https://x".to_string()),
            source_mcp: None,
            path_count: Some(3),
            cached_at: None,
            enabled: true,
        });
        let s = serde_json::to_string(&out).expect("serialize");
        // Untagged: no "Registry" wrapper key.
        assert!(!s.contains("Registry"));
        assert!(s.contains(r#""name":"pets""#));
        assert!(s.contains(r#""enabled":true"#));
    }

    #[test]
    fn spec_update_output_sync_mcp_variant_is_untagged() {
        let out = SpecUpdateOutput::SyncMcp(SyncMcpSpecsResult {
            server: "srv".to_string(),
            synced: 4,
            errors: vec!["boom".to_string()],
        });
        let s = serde_json::to_string(&out).expect("serialize");
        assert!(!s.contains("SyncMcp"));
        assert!(s.contains(r#""server":"srv""#));
        assert!(s.contains(r#""synced":4"#));
        assert!(s.contains(r#""boom""#));
    }
}
