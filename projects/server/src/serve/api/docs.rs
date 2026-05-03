use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use super::prelude::*;
use crate::markdown::to_llm_text;
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
pub async fn search_handler(Query(params): Query<SearchQuery>) -> Response {
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
        for (path, matches) in brain_docs::search(&query) {
            let file_path = path.trim_end_matches(".md").replace('\\', "/").to_string();
            results.push(json!({ "root": "docs", "path": file_path, "matches": matches }));
        }
    }

    results.sort_by(|a, b| {
        let am = a["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        let bm = b["matches"].as_array().map(|a| a.len()).unwrap_or(0);
        bm.cmp(&am)
    });
    results.truncate(20);
    Json(results).into_response()
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
        return match brain_docs::read(&params.path) {
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
