use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use brain_utils::config::Config;
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use super::prelude::*;

// ── Credential helper ─────────────────────────────────────────────────────────

/// Returns (domain, email, token) from the "atlassian" MCP server config in orca.db.
pub(super) fn atlassian_creds(config: &Config) -> anyhow::Result<(String, String, String)> {
    let conn = brain_utils::db::open(&config.db_path)?;
    let servers = brain_utils::db::list_mcp_servers(&conn)?;
    let server = servers
        .into_iter()
        .find(|s| s.name == "atlassian")
        .ok_or_else(|| anyhow::anyhow!("atlassian not configured — add via `orca mcp add`"))?;

    let args = &server.args;

    let domain = args
        .windows(2)
        .find(|w| w[0] == "--domain")
        .and_then(|w| w.get(1))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "rebuyengine.atlassian.net".to_string());

    let email = args
        .windows(2)
        .find(|w| w[0] == "--email")
        .and_then(|w| w.get(1))
        .map(|s| s.to_string())
        .or_else(|| server.env.get("ATLASSIAN_USERNAME").cloned())
        .ok_or_else(|| anyhow::anyhow!("no Atlassian email in server config"))?;

    let token = args
        .windows(2)
        .find(|w| w[0] == "--token")
        .and_then(|w| w.get(1))
        .map(|s| s.to_string())
        .or_else(|| server.env.get("ATLASSIAN_API_TOKEN").cloned())
        .ok_or_else(|| anyhow::anyhow!("no Atlassian token in server config"))?;

    Ok((domain, email, token))
}

// ── Jira ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct JiraIssuesQuery {
    pub jql: Option<String>,
    #[serde(rename = "maxResults")]
    pub max_results: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/jira/issues",
    operation_id = "listJiraIssues",
    params(
        ("jql" = Option<String>, Query, description = "JQL query (default: assignee = currentUser() ORDER BY updated DESC)"),
        ("maxResults" = Option<u32>, Query, description = "Max results to return (default: 50)"),
    ),
    responses(
        (status = 200, description = "Jira search result", body = serde_json::Value),
        (status = 500, description = "Config or credential error", body = ErrorResponse),
        (status = 502, description = "Upstream Jira API error", body = ErrorResponse),
    ),
    tag = "jira"
)]
pub async fn jira_issues_handler(Query(q): Query<JiraIssuesQuery>) -> impl IntoResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let (domain, email, token) = match atlassian_creds(&config) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let jql = q
        .jql
        .unwrap_or_else(|| "assignee = currentUser() ORDER BY updated DESC".to_string());
    let max = q.max_results.unwrap_or(50);
    let fields = "summary,status,priority,issuetype,assignee,reporter,updated";
    let max_str = max.to_string();
    let url = format!("https://{domain}/rest/api/3/search");

    match reqwest::Client::new()
        .get(&url)
        .query(&[("jql", jql.as_str()), ("maxResults", max_str.as_str()), ("fields", fields)])
        .basic_auth(&email, Some(&token))
        .send()
        .await
    {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>().await {
                Ok(body) => {
                    if status.is_success() {
                        Json(body).into_response()
                    } else {
                        (StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                         Json(body)).into_response()
                    }
                }
                Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
            }
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub struct TransitionBody {
    #[serde(rename = "transitionId")]
    pub transition_id: String,
}

#[utoipa::path(
    get,
    path = "/api/jira/issues/{key}/transitions",
    operation_id = "getJiraTransitions",
    params(
        ("key" = String, Path, description = "Jira issue key (e.g. PROJ-123)"),
    ),
    responses(
        (status = 200, description = "Available transitions for the issue", body = serde_json::Value),
        (status = 500, description = "Config or credential error", body = ErrorResponse),
        (status = 502, description = "Upstream Jira API error", body = ErrorResponse),
    ),
    tag = "jira"
)]
pub async fn jira_get_transitions_handler(Path(key): Path<String>) -> impl IntoResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let (domain, email, token) = match atlassian_creds(&config) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let url = format!("https://{domain}/rest/api/3/issue/{key}/transitions");
    match reqwest::Client::new()
        .get(&url)
        .basic_auth(&email, Some(&token))
        .send()
        .await
    {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => match resp.json::<Value>().await {
            Ok(body) => Json(body).into_response(),
            Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
    }
}

#[utoipa::path(
    post,
    path = "/api/jira/issues/{key}/transitions",
    operation_id = "transitionJiraIssue",
    params(
        ("key" = String, Path, description = "Jira issue key (e.g. PROJ-123)"),
    ),
    request_body = TransitionBody,
    responses(
        (status = 200, description = "Transition applied", body = OkResponse),
        (status = 500, description = "Config or credential error", body = ErrorResponse),
        (status = 502, description = "Upstream Jira API error", body = ErrorResponse),
    ),
    tag = "jira"
)]
pub async fn jira_transition_handler(
    Path(key): Path<String>,
    Json(body): Json<TransitionBody>,
) -> impl IntoResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let (domain, email, token) = match atlassian_creds(&config) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let url = format!("https://{domain}/rest/api/3/issue/{key}/transitions");
    let payload = json!({ "transition": { "id": body.transition_id } });

    match reqwest::Client::new()
        .post(&url)
        .basic_auth(&email, Some(&token))
        .json(&payload)
        .send()
        .await
    {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Json(json!({ "ok": true })).into_response()
            } else {
                let text = resp.text().await.unwrap_or_default();
                err(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), &text)
            }
        }
    }
}

// ── Confluence ────────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct ConfluenceSearchQuery {
    pub cql: Option<String>,
    pub limit: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/confluence/search",
    operation_id = "searchConfluence",
    params(
        ("cql" = Option<String>, Query, description = "CQL query (default: type = page ORDER BY lastModified DESC)"),
        ("limit" = Option<u32>, Query, description = "Max results (default: 25)"),
    ),
    responses(
        (status = 200, description = "Confluence search results with expanded space and excerpt", body = serde_json::Value),
        (status = 500, description = "Config or credential error", body = ErrorResponse),
        (status = 502, description = "Upstream Confluence API error", body = ErrorResponse),
    ),
    tag = "confluence"
)]
pub async fn confluence_search_handler(
    Query(q): Query<ConfluenceSearchQuery>,
) -> impl IntoResponse {
    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let (domain, email, token) = match atlassian_creds(&config) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let cql = q
        .cql
        .unwrap_or_else(|| "type = page ORDER BY lastModified DESC".to_string());
    let limit = q.limit.unwrap_or(25);

    let limit_str = limit.to_string();
    let url = format!("https://{domain}/wiki/rest/api/content/search");

    match reqwest::Client::new()
        .get(&url)
        .query(&[("cql", cql.as_str()), ("limit", limit_str.as_str()), ("expand", "space,excerpt")])
        .basic_auth(&email, Some(&token))
        .send()
        .await
    {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>().await {
                Ok(mut body) => {
                    if status.is_success() {
                        // Fix relative _links.webui → full URL
                        if let Some(results) = body["results"].as_array_mut() {
                            for r in results.iter_mut() {
                                if let Some(webui) = r["_links"]["webui"].as_str() {
                                    let full = format!("https://{domain}/wiki{webui}");
                                    r["_links"]["webui"] = json!(full);
                                }
                            }
                        }
                        Json(body).into_response()
                    } else {
                        (StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                         Json(body)).into_response()
                    }
                }
                Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
            }
        }
    }
}
