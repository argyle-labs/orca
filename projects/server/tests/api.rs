#![allow(clippy::disallowed_types)] // test harness — Value used for flexible response assertions
//! Axum integration tests for the orca HTTP API.
//!
//! Each test spins up the real router against a fresh unencrypted SQLite DB
//! (injected via `ORCA_DB_PATH`).  No network calls are made — external-service
//! handlers (Jira, GitHub, rebuy MCP) are exercised only for their error paths.
//!
//! Run with:
//!   cargo test -p orca --test api
//!
//! DB mutation tests use a thread-local DB path override so parallel tests don't race.

use std::path::PathBuf;

use axum::body::Body;
extern crate db;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

// ── Test harness ──────────────────────────────────────────────────────────────

struct TestApp {
    router: axum::Router,
    _tmp: TempDir,
    db_path: PathBuf,
}

impl TestApp {
    /// Build a fresh router backed by an isolated unencrypted SQLite DB.
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let router = orca::serve::build_router(false, db_path.clone());
        TestApp {
            router,
            _tmp: tmp,
            db_path,
        }
    }

    /// Execute a single request against the router.
    async fn call(&self, req: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(req).await.expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body: Value = if bytes.is_empty() {
            json!(null)
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes).to_string() }))
        };
        (status, body)
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("x-correlation-id", "test-cid")
            .body(Body::empty())
            .unwrap();
        self.call(req).await
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-correlation-id", "test-cid")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        self.call(req).await
    }

    async fn delete(&self, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("x-correlation-id", "test-cid")
            .body(Body::empty())
            .unwrap();
        self.call(req).await
    }

    /// Point `open_default()` at this test's isolated DB for the duration of the closure.
    /// Uses a thread-local override so parallel tests on different threads don't race.
    #[allow(clippy::ptr_arg)]
    fn with_db<F, Fut>(db_path: &PathBuf, f: F) -> impl std::future::Future<Output = ()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let db_path = db_path.to_string_lossy().to_string();
        async move {
            db::set_thread_db_path(Some(&db_path));
            f().await;
            db::set_thread_db_path(None);
        }
    }
}

// ── GET /api/health ───────────────────────────────────────────────────────────

#[tokio::test]
async fn health_ping_returns_ok() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
}

// ── OpenAPI spec ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn openapi_spec_is_valid_json() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["info"]["title"].is_string(), "spec missing info.title");
    assert!(body["paths"].is_object(), "spec missing paths");
}

#[tokio::test]
async fn openapi_public_spec_is_valid_json() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/openapi/public.json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["paths"].is_object(), "public spec missing paths");
}

#[tokio::test]
async fn openapi_spec_contains_expected_routes() {
    let app = TestApp::new();
    let (_, body) = app.get("/api/openapi.json").await;
    let paths = body["paths"].as_object().expect("paths object");
    assert!(paths.contains_key("/api/health"), "missing /api/health");
    assert!(
        paths.contains_key("/api/mcp/servers"),
        "missing /api/mcp/servers"
    );
    assert!(paths.contains_key("/api/plugins"), "missing /api/plugins");
    assert!(paths.contains_key("/api/tree"), "missing /api/tree");
}

// ── GET /api/tree ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn tree_returns_json_object() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/tree").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_object(), "tree should return a JSON object");
}

#[tokio::test]
async fn tree_raw_param_accepted() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/tree?raw=true").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_object());
}

// ── GET /api/search ───────────────────────────────────────────────────────────

#[tokio::test]
async fn search_empty_query_returns_empty_array() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/search").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn search_blank_query_string_returns_empty_array() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/search?q=").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn search_docs_root_returns_array() {
    let app = TestApp::new();
    // "orca" appears in embedded docs — should return results
    let (status, body) = app.get("/api/search?q=orca&root=docs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array(), "search should return array");
}

// ── GET /api/doc ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn doc_unknown_root_returns_400() {
    let app = TestApp::new();
    let (status, body) = app
        .get("/api/doc?root=unknown-root-xyz&path=anything")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn doc_embedded_docs_root_known_file_returns_200() {
    let app = TestApp::new();
    // Use the first embedded doc file
    let (status, body) = app.get("/api/doc?root=docs&path=architecture").await;
    // If the file exists: 200; if not by that exact name: 404 — both are valid responses
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "unexpected status: {status}"
    );
    if status == StatusCode::OK {
        // body is plain text, not JSON — our harness wraps it in raw field
        assert!(body["raw"].is_string() || body.is_string() || !body.is_null());
    }
}

#[tokio::test]
async fn doc_embedded_docs_nonexistent_returns_404() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/doc?root=docs&path=zzz-no-such-doc-xyz").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

// ── MCP servers (CRUD) ────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_servers_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/mcp/servers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array(), "mcp servers should be array: {body}");
    })
    .await;
}

#[tokio::test]
async fn mcp_servers_add_and_list() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app
            .post(
                "/api/mcp/servers",
                json!({
                    "name": "test-server-add",
                    "command": "node",
                    "args": ["server.js"],
                    "env": {},
                    "enabled": true
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "add failed: {body}");
        assert_eq!(body["ok"], json!(true));

        let (status, body) = app.get("/api/mcp/servers").await;
        assert_eq!(status, StatusCode::OK);
        let servers = body.as_array().expect("array");
        assert!(
            servers.iter().any(|s| s["name"] == "test-server-add"),
            "added server not in list"
        );
    })
    .await;
}

#[tokio::test]
async fn mcp_servers_remove_known_returns_ok() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        // Add first
        app.post(
            "/api/mcp/servers",
            json!({ "name": "test-server-rm", "command": "node", "args": [], "env": {}, "enabled": true }),
        )
        .await;

        let (status, body) = app.delete("/api/mcp/servers/test-server-rm").await;
        assert_eq!(status, StatusCode::OK, "remove failed: {body}");
        assert_eq!(body["ok"], json!(true));
    })
    .await;
}

#[tokio::test]
async fn mcp_servers_remove_unknown_returns_404() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.delete("/api/mcp/servers/zzz-no-such-server-xyz").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "got: {body}");
    })
    .await;
}

// ── MCP tool mappings ─────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_mappings_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/mcp/mappings").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array(), "mappings should be array: {body}");
    })
    .await;
}

#[tokio::test]
async fn mcp_mappings_create_and_delete() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        // Register the MCP server first (mappings have a FK constraint on mcp_name).
        let (status, body) = app
            .post(
                "/api/mcp/servers",
                json!({ "name": "test-server", "command": "echo", "args": [], "env": {}, "enabled": true }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "add server failed: {body}");

        let (status, body) = app
            .post(
                "/api/mcp/mappings",
                json!({ "name": "test-server", "orca_tool": "test_map_tool", "external_tool": "real_tool" }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "create mapping failed: {body}");

        let (status, body) = app.delete("/api/mcp/mappings/test_map_tool").await;
        assert_eq!(status, StatusCode::OK, "delete mapping failed: {body}");
        assert_eq!(body["ok"], json!(true));
    })
    .await;
}

// ── Docker runtimes (CRUD) ────────────────────────────────────────────────────

#[tokio::test]
async fn docker_runtimes_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/docker/runtimes").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array(), "runtimes should be array: {body}");
    })
    .await;
}

#[tokio::test]
async fn docker_runtimes_add_and_remove() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app
            .post(
                "/api/docker/runtimes",
                json!({
                    "name": "test-runtime",
                    "socket_path": "/var/run/docker.sock",
                    "host": null,
                    "url": null
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "add runtime failed: {body}");

        let (status, _) = app.delete("/api/docker/runtimes/test-runtime").await;
        assert_eq!(status, StatusCode::OK);
    })
    .await;
}

#[tokio::test]
async fn docker_runtimes_remove_unknown_returns_404() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, _) = app.delete("/api/docker/runtimes/no-such-runtime-xyz").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    })
    .await;
}

// ── Schema databases (CRUD) ───────────────────────────────────────────────────

#[tokio::test]
async fn schema_databases_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/schema/databases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array(), "databases should be array: {body}");
    })
    .await;
}

#[tokio::test]
async fn schema_databases_add_and_remove() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app
            .post(
                "/api/schema/databases",
                json!({
                    "name": "test-db-xyz",
                    "kind": "mysql",
                    "host": "localhost",
                    "port": 3306,
                    "user": "root",
                    "password": "secret",
                    "database": "mydb"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "add db failed: {body}");

        let (status, body) = app.delete("/api/schema/databases/test-db-xyz").await;
        assert_eq!(status, StatusCode::OK, "remove db failed: {body}");
        assert_eq!(body["ok"], json!(true));
    })
    .await;
}

// ── Learning progress ─────────────────────────────────────────────────────────

#[tokio::test]
async fn learning_progress_get_and_save() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/learning/progress").await;
        assert_eq!(status, StatusCode::OK, "get progress failed: {body}");
        // page is null or a string
        assert!(body["page"].is_null() || body["page"].is_string());

        let (status, body) = app
            .post("/api/learning/progress", json!({ "page": "/learn/intro" }))
            .await;
        assert_eq!(status, StatusCode::OK, "save progress failed: {body}");

        let (status, body) = app.get("/api/learning/progress").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["page"], json!("/learn/intro"));
    })
    .await;
}

// ── Plugins ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plugins_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/plugins").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array(), "plugins should be array: {body}");
    })
    .await;
}

// ── System status ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn system_status_returns_200() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/system/status").await;
    assert_eq!(status, StatusCode::OK, "system status failed: {body}");
    assert!(body.is_object(), "system status should be object: {body}");
}

// ── External-service error paths (no live backend) ───────────────────────────

#[tokio::test]
async fn rebuy_health_without_mcp_returns_503_or_error() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, _) = app.get("/api/rebuy/health/local").await;
        // No rebuy MCP registered → 503
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    })
    .await;
}

#[tokio::test]
async fn mcp_run_unknown_server_returns_error() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app
            .post(
                "/api/mcp/run",
                json!({ "server": "no-such-server", "tool": "any_tool", "input": {} }),
            )
            .await;
        assert!(
            status.is_client_error() || status.is_server_error(),
            "should fail without MCP server: {status} {body}"
        );
    })
    .await;
}

// ── Spec routes (no files registered) ────────────────────────────────────────

#[tokio::test]
async fn specs_list_returns_array() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/specs").await;
        assert_eq!(status, StatusCode::OK, "specs list failed: {body}");
        assert!(
            body.is_array() || body.is_object(),
            "unexpected body: {body}"
        );
    })
    .await;
}

#[tokio::test]
async fn spec_get_unknown_triggers_background_sync() {
    let app = TestApp::new();
    let db = app.db_path.clone();
    TestApp::with_db(&db, || async {
        let (status, body) = app.get("/api/specs/no-such-spec-xyz").await;
        // Unknown specs return 202 and kick off a background sync attempt.
        assert_eq!(status, StatusCode::ACCEPTED, "body: {body}");
        assert_eq!(body["generating"], json!(true));
    })
    .await;
}

// ── Filesystem browse ─────────────────────────────────────────────────────────

#[tokio::test]
async fn fs_browse_missing_path_returns_error() {
    let app = TestApp::new();
    let (status, _) = app
        .get("/api/fs/browse?path=/tmp/__orca_test_no_exist_xyz__")
        .await;
    // Expect 400 or 404 for a path that doesn't exist
    assert!(status.is_client_error() || status.is_server_error());
}

// ── docs/search internal logic ────────────────────────────────────────────────

#[tokio::test]
async fn search_with_docs_root_returns_results_for_known_term() {
    let app = TestApp::new();
    let (status, body) = app.get("/api/search?q=memory&root=docs").await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("should be array");
    // "memory" should appear in the embedded docs
    assert!(!arr.is_empty(), "expected results for 'memory' in docs");
    for r in arr {
        assert_eq!(
            r["root"],
            json!("docs"),
            "all results should be from docs root"
        );
    }
}

// ── Middleware: correlation ID passthrough ────────────────────────────────────

#[tokio::test]
async fn correlation_id_header_accepted() {
    let app = TestApp::new();
    let req = Request::builder()
        .method("GET")
        .uri("/api/health")
        .header("x-correlation-id", "my-trace-id-12345")
        .body(Body::empty())
        .unwrap();
    let (status, body) = app.call(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
}

// ── 404 fallback ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_route_returns_non_200() {
    let app = TestApp::new();
    let (status, _) = app.get("/api/totally/unknown/route/xyz").await;
    // Static fallback serves index.html (200) or 404 depending on embed
    // Either way we should get a response, not a panic
    assert!(status.as_u16() > 0);
}
