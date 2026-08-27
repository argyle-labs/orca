// Free-form JSON assertions against real HTTP responses — `serde_json::Value`
// is intentional here (see common/mod.rs).
#![allow(clippy::disallowed_types)]

//! Integration tier: authenticated `/api/v1/*` tool surface.
//!
//! Drives the real axum router in-process with a minted bearer token, hitting
//! read-only / pure-compute tool handlers (no host mutation, no network, no
//! subprocess). Also covers the auth-failure paths: no token → 401, bad token
//! → 401, wrong-role → 403 (exercises `require_auth` + `require_tool_role`).

mod common;

use axum::http::StatusCode;
use common::{mint_admin_token, mint_token, oneshot_json, oneshot_raw, with_isolated_env};

// ── successful authenticated dispatch ───────────────────────────────────────

#[tokio::test]
async fn system_health_returns_report_with_admin_token() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.health",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    // HealthReport shape (camelCase): healthy/version/displayName/machineId/daemon.
    assert!(
        body["healthy"].is_boolean(),
        "healthy must be a bool: {body}"
    );
    assert!(
        body["version"].as_str().is_some_and(|v| !v.is_empty()),
        "version must be a non-empty string: {body}"
    );
    assert!(
        body.get("daemon").is_some(),
        "daemon runtime snapshot must be present: {body}"
    );
    assert!(
        body.get("machineId").is_some(),
        "machineId must be present: {body}"
    );
}

#[tokio::test]
async fn system_detail_capabilities_view_lists_capabilities() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.detail",
        Some(&token),
        Some(serde_json::json!({ "view": "capabilities" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    // Untagged Capabilities variant → { "capabilities": [ ... ] }.
    assert!(
        body["capabilities"].is_array(),
        "capabilities view must return a capabilities array: {body}"
    );
}

#[tokio::test]
async fn auth_token_list_round_trips_minted_token() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/auth.token.list",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let tokens = body["tokens"].as_array().expect("tokens array present");
    assert_eq!(tokens.len(), 1, "the one minted token should be listed");
    assert_eq!(tokens[0]["name"], serde_json::json!("integration"));
    assert_eq!(tokens[0]["role"], serde_json::json!("admin"));
    // The list surface never leaks plaintext or hash.
    assert!(
        !body.to_string().contains(&token),
        "token plaintext must not appear in the list body"
    );
}

// ── auth failure paths ──────────────────────────────────────────────────────

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let env = with_isolated_env();
    // A token exists in the DB, so the loopback bootstrap fallback stays closed
    // for non-token_create paths.
    let _admin = mint_admin_token(&env);
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/system.health",
        None,
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("auth required"),
        "401 body should explain the failure: {text}"
    );
}

#[tokio::test]
async fn bad_token_is_unauthorized() {
    let env = with_isolated_env();
    let _admin = mint_admin_token(&env);
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/system.health",
        Some("orca_not_a_real_token_deadbeef"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("auth required"), "got: {text}");
}

#[tokio::test]
async fn read_role_token_forbidden_on_admin_tool() {
    let env = with_isolated_env();
    // `system.health` derives REQUIRED_ROLE = "admin" (non read-shaped verb).
    // A read-role token authenticates (require_auth passes) but fails the
    // per-tool role gate (require_tool_role) → 403.
    let token = mint_token(&env, "read");
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/system.health",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expected 403 for read role");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("system.health") && text.contains("admin"),
        "403 body should name the tool + required role: {text}"
    );
}

#[tokio::test]
async fn read_role_token_allowed_on_any_role_tool() {
    let env = with_isolated_env();
    // `system.detail` derives REQUIRED_ROLE = "any" (read-shaped verb), so a
    // read-role token passes the role gate.
    let token = mint_token(&env, "read");
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.detail",
        Some(&token),
        Some(serde_json::json!({ "view": "capabilities" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "read role should pass on any-tool: {body}"
    );
    assert!(body["capabilities"].is_array(), "body: {body}");
}

#[tokio::test]
async fn unknown_tool_is_not_found() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    // Authenticated + admin, but the tool does not exist → registry 404.
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.no_such_tool",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
}
