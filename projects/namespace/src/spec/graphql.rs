//! GraphQL spec ops — local SDL introspection + Shopify Admin proxy.
//! `proxy_graphql` legitimately uses `serde_json::Value` (GraphQL request
//! variables and response bodies are arbitrary upstream JSON — documented
//! escape hatch).
#![allow(clippy::disallowed_types)] // GraphQL variables/response bodies are opaque upstream JSON.

use super::graphql_scanner::{
    self, GraphQlEnum as ScannerEnum, GraphQlField as ScannerField, GraphQlInfo as ScannerInfo,
    GraphQlOperation as ScannerOp, GraphQlType as ScannerType,
};
use super::registry::specs_dir;
use super::{
    GraphQlEnum, GraphQlField, GraphQlInfoData, GraphQlOperation, GraphQlType, GraphqlProxyResult,
};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

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

pub async fn graphql_info(repo: &str) -> Result<GraphQlInfoData> {
    if !validate_repo(repo) {
        return Err(anyhow!("invalid repo name"));
    }
    let path = specs_dir().join(format!("{repo}.graphql"));
    let sdl = std::fs::read_to_string(&path)
        .with_context(|| format!("no GraphQL schema for '{repo}'"))?;
    let info = graphql_scanner::parse_graphql_sdl(repo, &sdl)?;
    Ok(map_info(info))
}

pub async fn proxy_graphql(
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
