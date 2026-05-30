//! Shopify Admin GraphQL passthrough. The variables and response body are
//! arbitrary upstream JSON (GraphQL response shapes vary per query) — the
//! documented opaque-payload escape hatch.
#![allow(clippy::disallowed_types)] // GraphQL request variables + response body are opaque upstream JSON.

use anyhow::{Context, Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `body` is opaque — GraphQL response shapes vary per query and are not owned by orca.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct GraphqlProxyResult {
    pub status: u16,
    pub body: Value,
}

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
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
