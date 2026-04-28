use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use super::{Ctx7Response, McpState, err};
use crate::serve::middleware::CorrelationId;

// ── GET /api/ctx7 ─────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct Ctx7Query {
    pub q: String,
    pub topic: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/ctx7",
    operation_id = "getLibraryDocs",
    params(
        ("q" = String, Query, description = "Library name to look up (npm package, crate, etc.)"),
        ("topic" = Option<String>, Query, description = "Specific topic or function to focus on"),
    ),
    responses(
        (status = 200, description = "Library documentation", body = Ctx7Response),
        (status = 400, description = "Missing query", body = super::ErrorResponse),
        (status = 404, description = "Library not found", body = super::ErrorResponse),
        (status = 503, description = "context7 not available", body = super::ErrorResponse),
    ),
    tag = "library"
)]
pub async fn ctx7_handler(
    Query(params): Query<Ctx7Query>,
    State(pool): State<McpState>,
    Extension(CorrelationId(cid)): Extension<CorrelationId>,
) -> Response {
    if params.q.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "q required");
    }
    let Some(server) = pool.find_ctx7_server().await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "context7 not registered — add it to ~/.claude.json mcpServers",
        );
    };
    let Ok(client) = pool.get_or_connect(&server).await else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "could not connect to context7",
        );
    };

    let resolve_result = match client
        .call_tool(
            "resolve-library-id",
            json!({ "libraryName": params.q }),
            &cid,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let resolve_text = resolve_result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let Some((lib_id, lib_title)) = extract_library_id(&resolve_text) else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("No library found for \"{}\"", params.q),
        );
    };

    let mut docs_args = json!({ "context7CompatibleLibraryID": lib_id, "tokens": 8000 });
    if let Some(ref topic) = params.topic {
        docs_args["topic"] = json!(topic);
    }

    match client.call_tool("get-library-docs", docs_args, &cid).await {
        Ok(result) => {
            let content = result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
            Json(Ctx7Response {
                library_id: lib_id,
                title: lib_title,
                topic: params.topic,
                content,
            })
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub(crate) fn extract_library_id(text: &str) -> Option<(String, String)> {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(id) = parsed["libraries"][0]["id"].as_str()
    {
        let title = parsed["libraries"][0]["name"]
            .as_str()
            .unwrap_or("")
            .to_string();
        return Some((id.to_string(), title));
    }
    let re = regex::Regex::new(r"/[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+").ok()?;
    let m = re.find(text)?;
    let id = m.as_str().to_string();
    let title = id
        .split('/')
        .rfind(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    Some((id, title))
}
