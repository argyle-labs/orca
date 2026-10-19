// Free-form JSON assertions against real HTTP responses — `serde_json::Value`
// is intentional here (see common/mod.rs).
#![allow(clippy::disallowed_types)]

//! Integration tier: OPEN (unauthenticated) daemon HTTP endpoints.
//!
//! Drives the real axum router in-process and asserts status + body shape for
//! every route that `require_auth` lets through without a credential.

mod common;

use axum::http::StatusCode;
use common::{oneshot_json, oneshot_raw, with_isolated_env};

#[tokio::test]
async fn health_reports_ok_true() {
    let env = with_isolated_env();
    let (status, body) = oneshot_json(env.router(), "GET", "/api/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], serde_json::json!(true));
}

#[tokio::test]
async fn mcp_catalog_has_version_and_tools() {
    let env = with_isolated_env();
    // NOTE: `/api/mcp/catalog` is NOT an open path — it is gated by `require_auth`
    // (only `/api/health`, `/api/auth/bootstrap`, and
    // the web-auth routes are open). The mcp-serve stdio bridge reads it with the
    // loopback bearer token, so we authenticate here too.
    let token = common::mint_admin_token(&env);
    let (status, body) =
        oneshot_json(env.router(), "GET", "/api/mcp/catalog", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    // Version is the compiled crate version — a non-empty string.
    let version = body["version"].as_str().expect("version is a string");
    assert!(!version.is_empty(), "version must not be empty");
    // Tools is an array with real tool entries (registry force-linked via dev-deps).
    let tools = body["tools"].as_array().expect("tools is an array");
    assert!(
        !tools.is_empty(),
        "tool catalog should be populated: got {} tools",
        tools.len()
    );
    // Every tool entry carries a name field.
    assert!(
        tools.iter().all(|t| t.get("name").is_some()),
        "each tool entry must have a name"
    );
}

#[tokio::test]
async fn bootstrap_status_available_on_loopback_with_no_tokens() {
    let env = with_isolated_env();
    // Fresh DB, zero tokens, loopback peer → bootstrap is available.
    let (status, body) = oneshot_json(env.router(), "GET", "/api/auth/bootstrap", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], serde_json::json!(true));
}

#[tokio::test]
async fn bootstrap_status_unavailable_once_a_token_exists() {
    let env = with_isolated_env();
    let _tok = common::mint_admin_token(&env);
    let (status, body) = oneshot_json(env.router(), "GET", "/api/auth/bootstrap", None, None).await;
    assert_eq!(status, StatusCode::OK);
    // A token now exists → bootstrap closes.
    assert_eq!(body["available"], serde_json::json!(false));
}

#[tokio::test]
async fn docs_json_requires_auth() {
    let env = with_isolated_env();
    let _tok = common::mint_admin_token(&env);
    let (status, _) = oneshot_raw(env.router(), "GET", "/docs.json", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn docs_json_served_to_read_role() {
    let env = with_isolated_env();
    let token = common::mint_token(&env, "read");
    let (status, body) = oneshot_json(env.router(), "GET", "/docs.json", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["openapi"].as_str().is_some(),
        "spec must declare an openapi version: {body}"
    );
    assert!(
        body.get("paths").is_some(),
        "spec must carry a paths object"
    );
}

fn prod_router(env: &common::IsolatedEnv) -> axum::Router {
    orca::serve::build_router(false, env.db_path.clone())
}

#[tokio::test]
async fn old_spec_routes_are_gone() {
    let env = with_isolated_env();
    let token = common::mint_admin_token(&env);
    for path in ["/api/openapi.json", "/api/openapi/public.json", "/scalar"] {
        for bearer in [None, Some(token.as_str())] {
            let (status, bytes) = oneshot_raw(prod_router(&env), "GET", path, bearer, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert!(
                !String::from_utf8_lossy(&bytes).contains("\"openapi\""),
                "{path}"
            );
        }
    }
}

#[tokio::test]
async fn prefix_confusion_paths_are_not_public() {
    let env = with_isolated_env();
    let _tok = common::mint_admin_token(&env);
    for path in [
        "/api/healthX",
        "/api/health/x",
        "/api/health/../v1/auth.token.list",
        "/api/auth/web/signinX",
        "/api/auth/bootstrap/x",
        "/docs.jsonX",
        "/docsX",
        "/docs.json/x",
        "/docs/../docs.json",
    ] {
        let (status, bytes) = oneshot_raw(prod_router(&env), "GET", path, None, None).await;
        assert_ne!(status, StatusCode::OK, "{path}");
        assert!(
            !String::from_utf8_lossy(&bytes).contains("\"openapi\""),
            "{path}"
        );
    }
    let (status, _) = oneshot_raw(
        prod_router(&env),
        "POST",
        "/api/health/../v1/auth.token.list",
        None,
        Some(serde_json::json!({})),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn catalog_returns_json_document() {
    let env = with_isolated_env();
    // Also auth-gated (consumed by the `orca unit` CLI with a bearer token).
    let token = common::mint_admin_token(&env);
    let (status, body) =
        oneshot_json(env.router(), "GET", "/api/catalog", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    // The unit catalog is a JSON object/array — assert it decoded to a container.
    assert!(
        body.is_object() || body.is_array(),
        "catalog should be a JSON object or array, got: {body}"
    );
}

#[tokio::test]
async fn docs_serves_html_reference_unauthenticated() {
    let env = with_isolated_env();
    let _tok = common::mint_admin_token(&env);
    let (status, bytes) = oneshot_raw(env.router(), "GET", "/docs", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(bytes).expect("docs body is utf-8");
    assert!(html.contains("<title>API Reference</title>"));
    assert!(html.contains(r#"data-url="/docs.json""#));
    assert!(html.contains(r#"id="orca-signin""#));
    assert!(!html.contains("\"paths\""), "shell must not embed the spec");
}

#[tokio::test]
async fn docs_auth_script_is_served() {
    let env = with_isolated_env();
    let (status, bytes) = oneshot_raw(env.router(), "GET", "/docs/auth.js", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&bytes).contains("/api/auth/web/signin"));
}

#[tokio::test]
async fn unmatched_api_paths_never_reach_the_ui_fallback() {
    let env = with_isolated_env();
    let token = common::mint_admin_token(&env);
    for router in [env.router(), prod_router(&env)] {
        for bearer in [None, Some(token.as_str())] {
            for path in ["/api/nope", "/api/openapi.json", "/api/health/x", "/api"] {
                let (status, _) = oneshot_raw(router.clone(), "GET", path, bearer, None).await;
                assert!(
                    status == StatusCode::NOT_FOUND || status == StatusCode::UNAUTHORIZED,
                    "{path}: {status}"
                );
            }
        }
    }
}
