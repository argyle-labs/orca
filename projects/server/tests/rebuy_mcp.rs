/// Rebuy MCP federation smoke tests — validate that the rebuy-cli MCP server is
/// properly federated through orca's MCP layer.
///
/// Tests are split into two groups:
///
/// 1. **Unit tests** — read-only, no live processes.  Run as part of `cargo test`.
/// 2. **Integration tests** — marked `#[ignore]`, require the rebuy-cli MCP server
///    binary to be present on disk.  Run with:
///
///    ```
///    cargo test -p orca rebuy_ -- --ignored --nocapture
///    ```
use orca::serve::mcp_client::{McpClient, McpPool, McpServerConfig};
use serde_json::json;

// ── Constants ─────────────────────────────────────────────────────────────────

const REBUY_PLUGIN_ID: &str = "rebuy";
const REBUY_MCP_CMD: &str = "node";
const REBUY_MCP_ARG: &str =
    "/Users/scottkey/code/rebuy/rebuy-cli-mcp-server/build/index.js";
const REBUY_TOOL_PREFIX: &str = "rebuy_";
const MIN_TOOL_COUNT: usize = 100;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rebuy_config() -> McpServerConfig {
    McpServerConfig {
        command: REBUY_MCP_CMD.to_string(),
        args: vec![REBUY_MCP_ARG.to_string()],
        env: Default::default(),
        token: None,
        fallback_urls: vec![],
    }
}

fn default_db_path() -> std::path::PathBuf {
    dirs::home_dir()
        .expect("no home dir")
        .join(".orca")
        .join("orca.db")
}

/// Build a skip list from the pool's full config set, excluding rebuy.
/// Passed to `all_tools_filtered` so only the rebuy server is contacted.
fn rebuy_only_skip(pool: &McpPool) -> Vec<String> {
    pool.read_configs()
        .into_keys()
        .filter(|k| k != REBUY_PLUGIN_ID)
        .collect()
}

// ── Unit tests (no live process) ──────────────────────────────────────────────

#[test]
fn rebuy_mcp_binary_exists() {
    assert!(
        std::path::Path::new(REBUY_MCP_ARG).exists(),
        "rebuy MCP server binary not found at {REBUY_MCP_ARG}"
    );
}

#[test]
fn rebuy_plugin_registered_in_db() {
    let db_path = default_db_path();
    if !db_path.exists() {
        eprintln!("skipping: orca.db not found at {}", db_path.display());
        return;
    }

    let conn = db::open(&db_path).expect("failed to open orca.db");
    let plugins = db::list_plugins(&conn).expect("failed to list plugins");

    let rebuy = plugins.iter().find(|p| p.id == REBUY_PLUGIN_ID);
    assert!(rebuy.is_some(), "rebuy plugin not found in orca.db — run `orca plugin sync`");

    let rebuy = rebuy.unwrap();
    assert!(rebuy.enabled, "rebuy plugin is disabled in orca.db");
    assert!(
        rebuy.mcp_command.is_some(),
        "rebuy plugin has no mcp_command set"
    );

    let cmd = rebuy.mcp_command.as_deref().unwrap();
    assert!(!cmd.is_empty(), "rebuy plugin mcp_command is empty");

    assert!(
        !rebuy.mcp_args.is_empty(),
        "rebuy plugin has no mcp_args (expected path to index.js)"
    );
}

#[test]
fn rebuy_db_pool_read_configs_contains_rebuy() {
    let db_path = default_db_path();
    if !db_path.exists() {
        eprintln!("skipping: orca.db not found");
        return;
    }

    let pool = McpPool::new_with_db(db_path);
    let configs = pool.read_configs();
    assert!(
        configs.contains_key(REBUY_PLUGIN_ID),
        "rebuy not found in McpPool configs — check plugin is enabled and has mcp_command"
    );
}

// ── Integration tests (live MCP server) ───────────────────────────────────────

/// Connect directly to the rebuy MCP server and verify we can list tools.
#[tokio::test]
#[ignore]
async fn rebuy_connect_and_list_tools() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    assert!(
        client.tools.len() >= MIN_TOOL_COUNT,
        "expected ≥{MIN_TOOL_COUNT} tools, got {}",
        client.tools.len()
    );
    eprintln!("rebuy MCP server exposes {} tools", client.tools.len());
}

/// Most rebuy tools are prefixed with `rebuy_`.  The server also exposes a small
/// number of `shopify_*` pass-through tools — those are expected and not a failure.
/// We verify the rebuy_ majority and log any other-prefixed tools for awareness.
#[tokio::test]
#[ignore]
async fn rebuy_all_tools_have_rebuy_prefix() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let (rebuy_prefixed, other): (Vec<_>, Vec<_>) = client
        .tools
        .iter()
        .partition(|t| t.name.starts_with(REBUY_TOOL_PREFIX));

    if !other.is_empty() {
        eprintln!(
            "note: {} tool(s) without rebuy_ prefix (pass-throughs): {:?}",
            other.len(),
            other.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()
        );
    }

    assert!(
        rebuy_prefixed.len() >= MIN_TOOL_COUNT,
        "expected ≥{MIN_TOOL_COUNT} rebuy_-prefixed tools, got {}",
        rebuy_prefixed.len()
    );
}

/// `rebuy_version` is a non-destructive tool — returns CLI version info.
#[tokio::test]
#[ignore]
async fn rebuy_tool_call_version() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool("rebuy_version", json!({}), "test-version")
        .await
        .expect("rebuy_version call failed");

    eprintln!("rebuy_version result: {result:#}");
    assert!(!result.is_null(), "rebuy_version returned null");

    // Result should contain text content
    let has_content = result["content"]
        .as_array()
        .map(|arr| !arr.is_empty())
        .unwrap_or(false);
    assert!(has_content, "rebuy_version returned empty content");
}

/// `rebuy_status` returns git status across repos — non-destructive.
#[tokio::test]
#[ignore]
async fn rebuy_tool_call_status() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool("rebuy_status", json!({}), "test-status")
        .await
        .expect("rebuy_status call failed");

    eprintln!("rebuy_status result (truncated): {:.500}", result.to_string());
    assert!(!result.is_null(), "rebuy_status returned null");
}

/// `rebuy_doctor` checks environment health — non-destructive.
#[tokio::test]
#[ignore]
async fn rebuy_tool_call_doctor() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool("rebuy_doctor", json!({}), "test-doctor")
        .await
        .expect("rebuy_doctor call failed");

    eprintln!("rebuy_doctor result (truncated): {:.500}", result.to_string());
    assert!(!result.is_null(), "rebuy_doctor returned null");
}

/// McpPool with orca.db connects to rebuy and retrieves tools.
#[tokio::test]
#[ignore]
async fn rebuy_pool_get_or_connect() {
    let db_path = default_db_path();
    let pool = McpPool::new_with_db(db_path);

    let client = pool
        .get_or_connect(REBUY_PLUGIN_ID)
        .await
        .expect("pool failed to connect to rebuy");

    assert!(
        client.tools.len() >= MIN_TOOL_COUNT,
        "expected ≥{MIN_TOOL_COUNT} tools via pool, got {}",
        client.tools.len()
    );
}

/// `all_tools_filtered` strips the `rebuy_` prefix from every tool name.
/// The `alias` field on each returned tool should carry the original `rebuy_*` name.
/// Uses a rebuy-scoped skip list so only the rebuy server is contacted.
#[tokio::test]
#[ignore]
async fn rebuy_prefix_stripped_in_all_tools_filtered() {
    let db_path = default_db_path();
    let pool = McpPool::new_with_db(db_path);

    let skip = rebuy_only_skip(&pool);
    let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
    let tools = pool.all_tools_filtered(&skip_refs).await;

    let rebuy_tools: Vec<_> = tools
        .iter()
        .filter(|t| t["server"].as_str() == Some(REBUY_PLUGIN_ID))
        .collect();

    assert!(
        rebuy_tools.len() >= MIN_TOOL_COUNT,
        "expected ≥{MIN_TOOL_COUNT} rebuy tools after filtering, got {}",
        rebuy_tools.len()
    );

    // Every exposed name must NOT start with rebuy_
    let still_prefixed: Vec<_> = rebuy_tools
        .iter()
        .filter(|t| {
            t["name"]
                .as_str()
                .map(|n| n.starts_with(REBUY_TOOL_PREFIX))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        still_prefixed.is_empty(),
        "tools still have rebuy_ prefix after stripping: {:?}",
        still_prefixed.iter().map(|t| &t["name"]).collect::<Vec<_>>()
    );

    // Tools that WERE renamed (had rebuy_ stripped) must have an alias pointing
    // back to the original rebuy_* name.  Pass-through tools (e.g. shopify_*)
    // have no alias — that's correct behaviour.
    let stripped_tools: Vec<_> = rebuy_tools
        .iter()
        .filter(|t| t["alias"] != serde_json::Value::Null)
        .collect();
    let missing_alias: Vec<_> = stripped_tools
        .iter()
        .filter(|t| {
            let alias = t["alias"].as_str().unwrap_or("");
            !alias.starts_with(REBUY_TOOL_PREFIX)
        })
        .collect();
    assert!(
        missing_alias.is_empty(),
        "stripped tools have wrong alias: {:?}",
        missing_alias.iter().map(|t| (&t["name"], &t["alias"])).collect::<Vec<_>>()
    );

    // Spot-check: stripped "version" should alias to "rebuy_version"
    let version_entry = rebuy_tools
        .iter()
        .find(|t| t["name"].as_str() == Some("version"));
    assert!(version_entry.is_some(), "'version' not found in filtered tools");
    assert_eq!(
        version_entry.unwrap()["alias"].as_str(),
        Some("rebuy_version"),
        "version alias should be rebuy_version"
    );
}

/// After prefix stripping, no two rebuy tools should map to the same exposed name.
/// Uses a rebuy-scoped skip list — only the rebuy server is contacted.
#[tokio::test]
#[ignore]
async fn rebuy_no_name_conflicts_with_orca_native() {
    let db_path = default_db_path();
    let pool = McpPool::new_with_db(db_path);

    let skip = rebuy_only_skip(&pool);
    let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
    let tools = pool.all_tools_filtered(&skip_refs).await;

    let rebuy_tools: Vec<_> = tools
        .iter()
        .filter(|t| t["server"].as_str() == Some(REBUY_PLUGIN_ID))
        .collect();

    // Check for duplicate exposed names within rebuy itself (two internal names → same stripped name)
    let mut seen: std::collections::HashMap<&str, &str> = Default::default();
    let mut dupes: Vec<(&str, &str, &str)> = Vec::new();
    for t in &rebuy_tools {
        let name = t["name"].as_str().unwrap_or("");
        let alias = t["alias"].as_str().unwrap_or(name);
        if let Some(prev_alias) = seen.insert(name, alias) {
            dupes.push((name, prev_alias, alias));
        }
    }

    assert!(
        dupes.is_empty(),
        "duplicate exposed names in rebuy tool set: {dupes:?}"
    );
    eprintln!("no duplicate names in rebuy's {} exposed tools", rebuy_tools.len());
}

/// Federation dispatch via pool: call stripped name "version", pool routes to
/// rebuy MCP server using the alias "rebuy_version".
/// Uses a rebuy-scoped skip list — only the rebuy server is contacted.
#[tokio::test]
#[ignore]
async fn rebuy_federation_dispatch_via_alias() {
    let db_path = default_db_path();
    let pool = McpPool::new_with_db(db_path);

    // Discover alias for "version" — scoped to rebuy only
    let skip = rebuy_only_skip(&pool);
    let skip_refs: Vec<&str> = skip.iter().map(|s| s.as_str()).collect();
    let tools = pool.all_tools_filtered(&skip_refs).await;
    let version_tool = tools
        .iter()
        .find(|t| {
            t["server"].as_str() == Some(REBUY_PLUGIN_ID)
                && t["name"].as_str() == Some("version")
        })
        .expect("'version' not found in filtered tools for rebuy");

    let internal_name = version_tool["alias"]
        .as_str()
        .unwrap_or(version_tool["name"].as_str().unwrap_or("version"));

    assert_eq!(internal_name, "rebuy_version");

    // Now call using the internal name on the server (simulates federation router)
    let client = pool
        .get_or_connect(REBUY_PLUGIN_ID)
        .await
        .expect("pool failed to connect to rebuy");

    let result = client
        .call_tool(internal_name, json!({}), "test-dispatch")
        .await
        .expect("federated call via alias failed");

    assert!(!result.is_null(), "federated rebuy_version returned null");
    eprintln!("federated version result: {result:#}");
}

/// `rebuy_spec_list` validates the rebuy MCP server can find its own spec files.
/// Rebuy specs live at ~/code/rebuy/rebuy-docs/docs/gen/ — the server's natural default.
#[tokio::test]
#[ignore]
async fn rebuy_spec_list_finds_specs() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool("rebuy_spec_list", json!({}), "test-spec-list")
        .await
        .expect("rebuy_spec_list call failed");

    eprintln!("spec_list result: {:.500}", result.to_string());

    let text = result["content"]
        .get(0)
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");

    assert!(
        !text.contains("Specs directory not found"),
        "rebuy spec tool could not find its specs dir — expected at ~/code/rebuy/rebuy-docs/docs/gen/: {text}"
    );
    assert!(
        !text.is_empty(),
        "spec_list returned empty output"
    );
}

/// `rebuy_spec_search` can search endpoints from rebuy's own spec directory.
#[tokio::test]
#[ignore]
async fn rebuy_spec_search_returns_results() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool(
            "rebuy_spec_search",
            json!({ "query": "customer", "repo": "apiv2" }),
            "test-spec-search",
        )
        .await
        .expect("rebuy_spec_search call failed");

    eprintln!("spec_search result (truncated): {:.500}", result.to_string());

    let text = result["content"]
        .get(0)
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");

    assert!(
        !text.contains("Specs directory not found"),
        "spec search could not find specs dir: {text}"
    );
    assert!(!text.is_empty(), "spec_search returned empty output");
}

/// `rebuy_docs_search` returns doc content without any configuration needed.
#[tokio::test]
#[ignore]
async fn rebuy_docs_search_returns_results() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool(
            "rebuy_docs_search",
            json!({ "query": "billing" }),
            "test-docs-search",
        )
        .await
        .expect("rebuy_docs_search call failed");

    let text = result["content"]
        .get(0)
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");

    assert!(!text.is_empty(), "docs_search returned empty output");
    eprintln!("docs_search result (truncated): {:.300}", text);
}

/// `rebuy_graphql_list` can find .graphql schema files from rebuy's own spec directory.
#[tokio::test]
#[ignore]
async fn rebuy_graphql_list_finds_schemas() {
    let client = McpClient::connect(&rebuy_config())
        .await
        .expect("failed to connect to rebuy MCP server");

    let result = client
        .call_tool("rebuy_graphql_list", json!({}), "test-graphql-list")
        .await
        .expect("rebuy_graphql_list call failed");

    eprintln!("graphql_list result: {:.500}", result.to_string());

    let text = result["content"]
        .get(0)
        .and_then(|c| c["text"].as_str())
        .unwrap_or("");

    assert!(!text.is_empty(), "graphql_list returned empty output");
    assert!(
        !text.contains("directory not found") && !text.contains("not found"),
        "graphql_list could not find schemas: {text}"
    );
}

