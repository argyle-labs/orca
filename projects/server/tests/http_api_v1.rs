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

use axum::http::{HeaderValue, StatusCode};
use common::{
    mint_admin_token, mint_token, mint_token_with, oneshot_json, oneshot_raw,
    oneshot_raw_with_headers, with_isolated_env,
};

// ── successful authenticated dispatch ───────────────────────────────────────

#[tokio::test]
async fn system_health_sweeps_every_system_with_admin_token() {
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
    // No id => every system in the mesh. The LOCAL one is always the first row,
    // because it is the one report that needs no network to produce.
    let systems = body["systems"]
        .as_array()
        .unwrap_or_else(|| panic!("mesh sweep must return `systems`: {body}"));
    let local = systems
        .first()
        .unwrap_or_else(|| panic!("the local system is always reported: {body}"));
    let health = &local["health"];
    assert!(
        health["healthy"].is_boolean(),
        "healthy must be a bool: {body}"
    );
    assert!(
        health["version"].as_str().is_some_and(|v| !v.is_empty()),
        "version must be a non-empty string: {body}"
    );
    assert!(
        health.get("daemon").is_some(),
        "daemon runtime snapshot must be present: {body}"
    );
    assert!(
        health.get("machineId").is_some(),
        "machineId must be present: {body}"
    );
}

/// Naming ONE system by id answers with that system's bare `HealthReport` —
/// the shape every existing decoder of this verb already reads. The id is the
/// resource being asked about, not a host selector (#647).
#[tokio::test]
async fn system_health_by_id_returns_one_bare_report() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    // Resolve this system's own id from the sweep, then ask for it by name.
    let (_, sweep) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.health",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    let display_name = sweep["systems"][0]["health"]["displayName"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !display_name.is_empty(),
        "sweep must name the system: {sweep}"
    );

    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/system.health",
        Some(&token),
        Some(serde_json::json!({ "id": display_name })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body["healthy"].is_boolean(),
        "a single-system answer is a bare HealthReport: {body}"
    );
    assert!(
        body.get("systems").is_none(),
        "a single-system answer must NOT be a mesh sweep: {body}"
    );
}

/// A blank id is an invalid id, never "absent": on a mutation it would widen to
/// "no filter".
#[tokio::test]
async fn blank_id_is_rejected_naming_the_argument() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    for (tool, args) in [
        (
            "storage.mount.update",
            serde_json::json!({ "id": "", "enabled": false }),
        ),
        (
            "storage.mount.update",
            serde_json::json!({ "id": "   ", "enabled": false }),
        ),
    ] {
        let (status, body) = oneshot_json(
            env.router(),
            "POST",
            &format!("/api/v1/{tool}"),
            Some(&token),
            Some(args.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tool} {args}: {body}");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains(&format!("invalid args for {tool}: id:"))),
            "{tool} {args}: {body}"
        );
    }
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
    // `config.upsert` derives REQUIRED_ROLE = "admin" — it WRITES. A read-role
    // token authenticates (require_auth passes) but fails the per-tool role
    // gate (require_tool_role) → 403.
    //
    // This used to probe `system.health`, which derived "admin" only because
    // the classifier's read-shaped set was `list|detail|search` and nothing
    // else. A health probe changes nothing and requiring admin for it was the
    // gap, not the rule (#636) — so the test now names a verb that genuinely
    // mutates.
    let token = mint_token(&env, "read");
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/config.upsert",
        Some(&token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expected 403 for read role");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("config.upsert") && text.contains("admin"),
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

/// A blank `peer` selector is refused, never read as "run on this system".
#[tokio::test]
async fn blank_peer_selector_is_rejected() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    for peer in ["", "   "] {
        let (status, body) = oneshot_json(
            env.router(),
            "POST",
            "/api/v1/system.health",
            Some(&token),
            Some(serde_json::json!({ "peer": peer })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{peer:?}: {body}");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("peer: ")),
            "{peer:?}: {body}"
        );
    }
}

/// The body `peer` is validated even when the `X-Orca-Peer` header is given.
#[tokio::test]
async fn blank_body_peer_is_rejected_beside_a_header() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_with_peer_header(
        env.router(),
        &token,
        HeaderValue::from_static("host-a"),
        serde_json::json!({ "peer": "" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("peer: a blank"), "{body}");
}

#[tokio::test]
async fn blank_peer_header_is_rejected() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_with_peer_header(
        env.router(),
        &token,
        HeaderValue::from_static("   "),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("peer: a blank"), "{body}");
}

/// A header and body `peer` naming different systems is ambiguous, so neither wins.
#[tokio::test]
async fn conflicting_header_and_body_peer_is_rejected() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_with_peer_header(
        env.router(),
        &token,
        HeaderValue::from_static("host-a"),
        serde_json::json!({ "peer": "host-b" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("peer: "), "{body}");
}

#[tokio::test]
async fn non_utf8_peer_header_is_rejected_as_such() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_with_peer_header(
        env.router(),
        &token,
        HeaderValue::from_bytes(b"host-\xff").unwrap(),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("not valid UTF-8"), "{body}");
}

async fn oneshot_with_peer_header(
    router: axum::Router,
    token: &str,
    peer: HeaderValue,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let (status, bytes) = oneshot_raw_with_headers(
        router,
        "POST",
        "/api/v1/system.health",
        Some(token),
        Some(body),
        &[("x-orca-peer", peer)],
    )
    .await;
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn read_role_token_forbidden_on_secrets_detail() {
    let env = with_isolated_env();
    let token = mint_token(&env, "read");
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/secrets.detail",
        Some(&token),
        Some(serde_json::json!({ "name": "s" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "expected 403 for read role");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("secrets.detail") && text.contains("admin"),
        "403 body should name the tool + required role: {text}"
    );
}

#[tokio::test]
async fn can_mutate_read_token_forbidden_on_secrets_upsert() {
    let env = with_isolated_env();
    let token = mint_token_with(&env, "read", true);
    let (status, bytes) = oneshot_raw(
        env.router(),
        "POST",
        "/api/v1/secrets.upsert",
        Some(&token),
        Some(serde_json::json!({ "name": "s", "value": "v", "execute": true })),
    )
    .await;
    let text = String::from_utf8_lossy(&bytes);
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {text}");
}

#[tokio::test]
async fn mcp_tools_call_secrets_detail_refuses_read_token() {
    let env = with_isolated_env();
    let token = mint_token(&env, "read");
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/mcp",
        Some(&token),
        Some(serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "secrets.detail", "arguments": { "name": "s" } }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["error"]["code"], -32000, "body: {body}");
}

#[tokio::test]
async fn admin_token_reads_secret_value() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/secrets.upsert",
        Some(&token),
        Some(serde_json::json!({ "name": "s", "value": "v", "execute": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "upsert: {body}");
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/v1/secrets.detail",
        Some(&token),
        Some(serde_json::json!({ "name": "s" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail: {body}");
    assert_eq!(body["value"], "v", "body: {body}");
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

// ── HTTP JSON-RPC MCP endpoint (#538 P2 Phase 1) ────────────────────────────

#[tokio::test]
async fn mcp_endpoint_initialize_returns_server_info() {
    let env = with_isolated_env();
    let token = mint_admin_token(&env);
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/mcp",
        Some(&token),
        Some(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["result"]["serverInfo"]["name"], "orca");
    assert_eq!(body["id"], 1);
}

#[tokio::test]
async fn mcp_endpoint_requires_auth() {
    let env = with_isolated_env();
    // A token exists so bootstrap stays closed; no bearer → 401 before dispatch.
    let _admin = mint_admin_token(&env);
    let (status, _body) = oneshot_json(
        env.router(),
        "POST",
        "/api/mcp",
        None,
        Some(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mcp_endpoint_tools_call_enforces_role() {
    let env = with_isolated_env();
    // A read-role caller must be rejected by the re-applied per-tool RBAC on an
    // admin-gated tool — a JSON-RPC authz error (-32000), never a dispatch.
    let token = mint_token(&env, "read");
    let (status, body) = oneshot_json(
        env.router(),
        "POST",
        "/api/mcp",
        Some(&token),
        Some(serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "config.upsert", "arguments": {} }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["error"]["code"], -32000,
        "insufficient role must return an authz error: {body}"
    );
}
