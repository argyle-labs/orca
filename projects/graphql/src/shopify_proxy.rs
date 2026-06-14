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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_repo_accepts_safe_names() {
        assert!(validate_repo("acme-shopify-client"));
        assert!(validate_repo("a_b.c-1"));
        assert!(validate_repo("X"));
    }

    #[test]
    fn validate_repo_rejects_empty_and_punctuation() {
        assert!(!validate_repo(""));
        assert!(!validate_repo("../etc/passwd"));
        assert!(!validate_repo("name with space"));
        assert!(!validate_repo("a$b"));
    }

    // shopify_admin_version + proxy_graphql exercise process-global env
    // (HOME / ORCA_CONFIG). Bundle into one test so the mutations don't
    // race other tests in this binary.
    #[tokio::test]
    async fn shopify_version_default_and_override_and_proxy_invalid_repo() {
        // SAFETY: tests are single-threaded inside this function and no other
        // test in this crate reads ORCA_CONFIG.
        let prev = std::env::var("ORCA_CONFIG").ok();

        // 1. Missing file → fallback default.
        unsafe {
            std::env::set_var("ORCA_CONFIG", "/nonexistent/path/orca.toml");
        }
        assert_eq!(shopify_admin_version(), "2026-01");

        // 2. Malformed TOML → fallback default.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is not toml = [[").unwrap();
        unsafe {
            std::env::set_var("ORCA_CONFIG", &bad);
        }
        assert_eq!(shopify_admin_version(), "2026-01");

        // 3. Valid TOML without specs section → fallback default.
        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "[other]\nfoo = 1\n").unwrap();
        unsafe {
            std::env::set_var("ORCA_CONFIG", &empty);
        }
        assert_eq!(shopify_admin_version(), "2026-01");

        // 4. Valid TOML with specs.shopify_admin_version → override.
        let good = dir.path().join("good.toml");
        std::fs::write(&good, "[specs]\nshopify_admin_version = \"2025-10\"\n").unwrap();
        unsafe {
            std::env::set_var("ORCA_CONFIG", &good);
        }
        assert_eq!(shopify_admin_version(), "2025-10");

        // 5. proxy_graphql with an invalid repo short-circuits before HTTP.
        let err = match proxy_graphql("bad name", "s", "t", "{}", None, None).await {
            Ok(_) => panic!("expected invalid-repo error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("invalid repo"));

        // Restore env.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("ORCA_CONFIG", v),
                None => std::env::remove_var("ORCA_CONFIG"),
            }
        }
    }

    #[test]
    fn graphql_proxy_result_round_trips_through_serde() {
        let r = GraphqlProxyResult {
            status: 200,
            body: json!({"data": 1}),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: GraphqlProxyResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back.status, 200);
        assert_eq!(back.body, json!({"data": 1}));
    }
}
