use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use super::llm;
use super::prelude::*;
use crate::markdown::to_llm_text;
use crate::serve::middleware::CorrelationId;
use crate::serve::tree::{build_tree_raw, collect_all_files, get_roots, get_search_ignored};

// ── GET /api/tree ─────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct TreeQuery {
    /// Pass `true` to skip compaction and return the raw filesystem tree.
    pub raw: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/api/tree",
    operation_id = "getTree",
    params(
        ("raw" = Option<bool>, Query, description = "Skip compaction — return raw filesystem tree"),
    ),
    responses(
        (status = 200, description = "Document tree indexed by root name", body = serde_json::Value),
        (status = 500, description = "Error", body = ErrorResponse),
    ),
    tag = "docs"
)]
pub async fn tree_handler(Query(params): Query<TreeQuery>) -> impl IntoResponse {
    let raw = params.raw.unwrap_or(false);
    let mut result = serde_json::Map::new();
    // Iterate over the live root registry so tree output stays in lockstep
    // with `get_roots()`. Adding a root in tree.rs surfaces it here without
    // touching this handler.
    for name in get_roots().keys() {
        let tree = if raw {
            crate::serve::tree::get_root_tree_raw(name)
        } else {
            crate::serve::tree::get_root_tree(name)
        };
        result.insert(name.clone(), serde_json::to_value(tree).unwrap_or_default());
    }
    Json(serde_json::Value::Object(result))
}

// ── GET /api/search ───────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub root: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/search",
    operation_id = "searchDocs",
    params(
        ("q" = Option<String>, Query, description = "Search query"),
        ("root" = Option<String>, Query, description = "Limit search to a specific root (orca/rebuy)"),
    ),
    responses(
        (status = 200, description = "Search results", body = Vec<super::SearchResult>),
    ),
    tag = "docs"
)]
pub async fn search_handler(
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
    Query(params): Query<SearchQuery>,
) -> Response {
    let query = params.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Json(json!([])).into_response();
    }
    let root_filter = params.root.as_deref().unwrap_or("all");
    let roots = get_roots();
    let pattern = regex::escape(&query).to_lowercase();
    let mut results: Vec<serde_json::Value> = Vec::new();

    for (name, root_dir) in &roots {
        if root_filter != "all" && root_filter != name {
            continue;
        }
        let ignored = get_search_ignored(name);
        let canonical_root = root_dir.canonicalize().unwrap_or_else(|_| root_dir.clone());
        let tree = build_tree_raw(&canonical_root, &canonical_root, &ignored);
        let files = collect_all_files(&tree);
        for file in files {
            let full = canonical_root.join(&file.path);
            if let Ok(content) = std::fs::read_to_string(&full) {
                let matches: Vec<String> = content
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.to_lowercase().contains(&pattern))
                    .take(3)
                    .map(|(i, line)| format!("L{}: {}", i + 1, line.trim()))
                    .collect();
                if !matches.is_empty() {
                    let file_path = file
                        .path
                        .replace(".md", "")
                        .replace(".mdx", "")
                        .replace('\\', "/");
                    results.push(json!({ "root": name, "path": file_path, "matches": matches }));
                }
            }
        }
    }
    if root_filter == "all" || root_filter == "docs" {
        for (path, matches) in orca_docs::search(&query) {
            let file_path = path.trim_end_matches(".md").replace('\\', "/").to_string();
            results.push(json!({ "root": "docs", "path": file_path, "matches": matches }));
        }
    }

    // Call search_tools declared by enabled plugins
    let plugin_results = call_plugin_search_tools(&pool, &query, &cid).await;
    results.extend(plugin_results);

    results.sort_by(|a, b| {
        let am = a["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        let bm = b["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        bm.cmp(&am)
    });
    results.truncate(30);

    // Attempt LLM reranking when a local model is available.
    // Short timeout (3s) keeps UI search responsive; falls back to raw results silently.
    if !results.is_empty() && !query.trim().is_empty() {
        if let Some(local_llm) = llm::discover_local_llm().await {
            if let Some(reranked) = llm::rerank_results(&local_llm, &query, &results, 3000).await {
                if !reranked.is_empty() {
                    return Json(reranked).into_response();
                }
            }
        }
    }

    Json(results).into_response()
}

async fn call_plugin_search_tools(
    pool: &McpState,
    query: &str,
    cid: &str,
) -> Vec<serde_json::Value> {
    use orca_utils::db;
    let plugins = db::open_default()
        .and_then(|conn| db::list_plugins(&conn))
        .unwrap_or_default();

    let mut out = Vec::new();
    for plugin in plugins {
        if !plugin.enabled || plugin.search_tools.is_empty() { continue; }
        let Ok(client) = pool.get_or_connect(&plugin.id).await else { continue };
        for st in &plugin.search_tools {
            let args = serde_json::json!({ &st.arg: query });
            let Ok(resp) = client.call_tool(&st.tool, args, cid).await else { continue };
            let text = resp["content"]
                .get(0)
                .and_then(|c| c["text"].as_str())
                .unwrap_or("");
            parse_search_response(text, &st.root, &mut out);
        }
    }
    out
}

/// Parse a plugin search tool response into SearchResult-compatible JSON.
/// Tries JSON array `[{path, matches}]` first; falls back to text-headers format:
/// `### path\n  [Ln] content`
fn parse_search_response(text: &str, root: &str, out: &mut Vec<serde_json::Value>) {
    // Try standard JSON format first
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(text) {
        for item in arr {
            if item["path"].is_string() {
                out.push(serde_json::json!({
                    "root": root,
                    "path": item["path"],
                    "matches": item.get("matches").cloned().unwrap_or(serde_json::json!([])),
                }));
            }
        }
        return;
    }

    // Fall back to text-headers format: ### path\n  [Ln] content
    let mut current_path: Option<&str> = None;
    let mut snippets: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if let Some(header) = line.strip_prefix("### ") {
            if let Some(p) = current_path.take() {
                if !snippets.is_empty() {
                    out.push(serde_json::json!({ "root": root, "path": p, "matches": snippets }));
                    snippets = Vec::new();
                }
            }
            current_path = Some(header.trim_end_matches(" [inline-docs]"));
        } else {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix('[') {
                if let Some(end) = rest.find(']') {
                    let content = rest[end + 1..].trim();
                    if !content.is_empty() { snippets.push(content.to_string()); }
                }
            }
        }
        // Flush last entry at end
        if i == lines.len() - 1 {
            if let Some(p) = current_path.take() {
                if !snippets.is_empty() {
                    out.push(serde_json::json!({ "root": root, "path": p, "matches": snippets }));
                }
            }
        }
    }
}

// ── GET /api/doc ──────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct DocQuery {
    pub root: String,
    pub path: String,
    /// Pass `llm` to strip decorative markdown syntax (bold, italic, images, HRs) and collapse
    /// whitespace. Reduces token usage when the content will be consumed by a language model.
    pub format: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/doc",
    operation_id = "getDoc",
    params(
        ("root" = String, Query, description = "Vault root name (orca/rebuy/docs)"),
        ("path" = String, Query, description = "File path relative to root"),
        ("format" = Option<String>, Query, description = "Pass `llm` to strip decorative markdown (bold, italic, images, HRs) and collapse whitespace — reduces token usage when the content will be read by a language model"),
    ),
    responses(
        (status = 200, description = "Document content as plain text", content_type = "text/plain"),
        (status = 400, description = "Unknown root", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse),
    ),
    tag = "docs"
)]
pub async fn doc_handler(Query(params): Query<DocQuery>) -> Response {
    let llm_mode = params.format.as_deref() == Some("llm");

    let apply = |content: String| -> String {
        if llm_mode { to_llm_text(&content) } else { content }
    };

    if params.root == "docs" {
        return match orca_docs::read(&params.path) {
            Some(content) => (
                StatusCode::OK,
                [("content-type", "text/plain; charset=utf-8")],
                apply(content),
            )
                .into_response(),
            None => err(StatusCode::NOT_FOUND, "not found"),
        };
    }

    let roots = get_roots();
    let Some(root_dir) = roots.get(&params.root) else {
        return err(StatusCode::BAD_REQUEST, "unknown root");
    };
    // Try exact path, then with .md / .mdx extensions (sidebar strips extensions from URLs)
    let candidates = [
        root_dir.join(&params.path),
        root_dir.join(format!("{}.md", params.path)),
        root_dir.join(format!("{}.mdx", params.path)),
    ];
    for full in &candidates {
        if !full.starts_with(root_dir) {
            return err(StatusCode::FORBIDDEN, "path traversal");
        }
        if let Ok(content) = std::fs::read_to_string(full) {
            return (
                StatusCode::OK,
                [("content-type", "text/plain; charset=utf-8")],
                apply(content),
            )
                .into_response();
        }
    }
    err(StatusCode::NOT_FOUND, "not found")
}
