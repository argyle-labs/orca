use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use orca_utils::config::Config;
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::ToSchema;

use super::prelude::*;

// ── Auth resolution ───────────────────────────────────────────────────────────

enum AtlassianAuth {
    /// OAuth 2.0 Bearer token from `orca login atlassian`.
    OAuth {
        access_token: String,
        cloud_id: String,
    },
    /// Classic API key (Basic auth) from MCP server config or env vars.
    ApiKey {
        domain: String,
        email: String,
        token: String,
    },
}

impl AtlassianAuth {
    fn jira_base(&self) -> String {
        match self {
            Self::OAuth { cloud_id, .. } => {
                format!("https://api.atlassian.com/ex/jira/{cloud_id}/rest/api/3")
            }
            Self::ApiKey { domain, .. } => format!("https://{domain}/rest/api/3"),
        }
    }

    fn confluence_base(&self) -> String {
        match self {
            Self::OAuth { cloud_id, .. } => {
                format!("https://api.atlassian.com/ex/confluence/{cloud_id}/wiki/rest/api")
            }
            Self::ApiKey { domain, .. } => format!("https://{domain}/wiki/rest/api"),
        }
    }

    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::OAuth { access_token, .. } => req.bearer_auth(access_token),
            Self::ApiKey { email, token, .. } => req.basic_auth(email, Some(token)),
        }
    }

}

#[derive(Deserialize)]
struct AccessibleResource {
    id: String,
}

/// Resolve credentials: try OAuth DB token first (with auto-refresh), fall back to API key config.
async fn resolve_auth() -> anyhow::Result<AtlassianAuth> {
    if let Some(access_token) = orca_commands::oauth::load_atlassian_access_token() {
        // Try the stored access token; if 401, attempt refresh.
        let access_token = match try_or_refresh_atlassian(access_token).await {
            Ok(t) => t,
            Err(e) => return Err(e),
        };

        let resources: Vec<AccessibleResource> = reqwest::Client::new()
            .get("https://api.atlassian.com/oauth/token/accessible-resources")
            .bearer_auth(&access_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let cloud_id = resources
            .into_iter()
            .next()
            .map(|r| r.id)
            .ok_or_else(|| anyhow::anyhow!("no Atlassian sites found for this OAuth token"))?;

        return Ok(AtlassianAuth::OAuth { access_token, cloud_id });
    }

    // Fall back to MCP server config or env vars
    let (domain, email, token) = api_key_creds()?;
    Ok(AtlassianAuth::ApiKey { domain, email, token })
}

#[derive(Deserialize)]
struct AtlassianRefreshResponse {
    access_token: String,
}

/// Try the token against accessible-resources; if it 401s, use the refresh token to get a new one.
async fn try_or_refresh_atlassian(access_token: String) -> anyhow::Result<String> {
    let probe = reqwest::Client::new()
        .get("https://api.atlassian.com/oauth/token/accessible-resources")
        .bearer_auth(&access_token)
        .send()
        .await?;

    if probe.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(access_token);
    }

    // Access token expired — try refresh
    let refresh_token = orca_commands::oauth::load_atlassian_refresh_token()
        .ok_or_else(|| anyhow::anyhow!("Atlassian token expired and no refresh token stored — run `orca login atlassian`"))?;

    let client_id = std::env::var("ATLASSIAN_OAUTH_CLIENT_ID")
        .map_err(|_| anyhow::anyhow!("ATLASSIAN_OAUTH_CLIENT_ID not set — cannot refresh token"))?;
    let client_secret = std::env::var("ATLASSIAN_OAUTH_CLIENT_SECRET")
        .map_err(|_| anyhow::anyhow!("ATLASSIAN_OAUTH_CLIENT_SECRET not set"))?;

    let resp: AtlassianRefreshResponse = reqwest::Client::new()
        .post("https://auth.atlassian.com/oauth/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
            ("refresh_token", &refresh_token),
        ])
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("token refresh failed: {e}"))?
        .json()
        .await?;

    // Persist the new access token
    let _ = orca_commands::oauth::update_atlassian_access_token(&resp.access_token);

    Ok(resp.access_token)
}

/// Read API key credentials from MCP server config, then env vars.
fn api_key_creds() -> anyhow::Result<(String, String, String)> {
    // Try MCP server config
    if let Ok(config) = Config::load() {
        if let Ok(conn) = orca_utils::db::open(&config.db_path) {
            if let Ok(servers) = orca_utils::db::list_mcp_servers(&conn) {
                if let Some(server) = servers.into_iter().find(|s| s.name == "atlassian") {
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
                        .or_else(|| server.env.get("ATLASSIAN_USERNAME").cloned());
                    let token = args
                        .windows(2)
                        .find(|w| w[0] == "--token")
                        .and_then(|w| w.get(1))
                        .map(|s| s.to_string())
                        .or_else(|| server.env.get("ATLASSIAN_API_TOKEN").cloned());
                    if let (Some(email), Some(token)) = (email, token) {
                        return Ok((domain, email, token));
                    }
                }
            }
        }
    }

    // Try environment variables
    let domain = std::env::var("ATLASSIAN_DOMAIN")
        .unwrap_or_else(|_| "rebuyengine.atlassian.net".to_string());
    let email = std::env::var("ATLASSIAN_USERNAME")
        .or_else(|_| std::env::var("ATLASSIAN_EMAIL"))
        .map_err(|_| anyhow::anyhow!(
            "no Atlassian credentials — run `orca login atlassian` (OAuth) or set ATLASSIAN_USERNAME + ATLASSIAN_API_TOKEN"
        ))?;
    let token = std::env::var("ATLASSIAN_API_TOKEN")
        .map_err(|_| anyhow::anyhow!("ATLASSIAN_API_TOKEN not set"))?;
    Ok((domain, email, token))
}

/// Kept for Bitbucket (which uses Basic auth regardless of OAuth).
pub(super) fn atlassian_creds(config: &Config) -> anyhow::Result<(String, String, String)> {
    api_key_creds().map_err(|_| {
        let _ = config;
        anyhow::anyhow!(
            "no Atlassian API key credentials — set ATLASSIAN_USERNAME + ATLASSIAN_API_TOKEN or configure via `orca mcp add atlassian`"
        )
    })
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
    let auth = match resolve_auth().await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let jql = q.jql.unwrap_or_else(|| "assignee = currentUser() ORDER BY updated DESC".to_string());
    let max = q.max_results.unwrap_or(50).to_string();
    let fields = "summary,status,priority,issuetype,assignee,reporter,updated";
    let url = format!("{}/search", auth.jira_base());

    let req = auth.apply(
        reqwest::Client::new()
            .get(&url)
            .query(&[("jql", jql.as_str()), ("maxResults", max.as_str()), ("fields", fields)]),
    );

    atlassian_json_response(req.send().await).await
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
    params(("key" = String, Path, description = "Jira issue key (e.g. PROJ-123)")),
    responses(
        (status = 200, description = "Available transitions", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
        (status = 502, body = ErrorResponse),
    ),
    tag = "jira"
)]
pub async fn jira_get_transitions_handler(Path(key): Path<String>) -> impl IntoResponse {
    let auth = match resolve_auth().await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let url = format!("{}/issue/{key}/transitions", auth.jira_base());
    atlassian_json_response(auth.apply(reqwest::Client::new().get(&url)).send().await).await
}

#[utoipa::path(
    post,
    path = "/api/jira/issues/{key}/transitions",
    operation_id = "transitionJiraIssue",
    params(("key" = String, Path, description = "Jira issue key")),
    request_body = TransitionBody,
    responses(
        (status = 200, body = OkResponse),
        (status = 500, body = ErrorResponse),
        (status = 502, body = ErrorResponse),
    ),
    tag = "jira"
)]
pub async fn jira_transition_handler(
    Path(key): Path<String>,
    Json(body): Json<TransitionBody>,
) -> impl IntoResponse {
    let auth = match resolve_auth().await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let url = format!("{}/issue/{key}/transitions", auth.jira_base());
    let payload = json!({ "transition": { "id": body.transition_id } });

    match auth.apply(reqwest::Client::new().post(&url).json(&payload)).send().await {
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
        (status = 200, description = "Confluence search results", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
        (status = 502, body = ErrorResponse),
    ),
    tag = "confluence"
)]
pub async fn confluence_search_handler(Query(q): Query<ConfluenceSearchQuery>) -> impl IntoResponse {
    let auth = match resolve_auth().await {
        Ok(a) => a,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let cql = q.cql.unwrap_or_else(|| "type = page ORDER BY lastModified DESC".to_string());
    let limit = q.limit.unwrap_or(25).to_string();
    let url = format!("{}/content/search", auth.confluence_base());

    let req = auth.apply(
        reqwest::Client::new()
            .get(&url)
            .query(&[("cql", cql.as_str()), ("limit", limit.as_str()), ("expand", "space,excerpt")]),
    );

    let domain_for_links = match &auth {
        AtlassianAuth::ApiKey { domain, .. } => Some(domain.clone()),
        AtlassianAuth::OAuth { .. } => None,
    };

    match req.send().await {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>().await {
                Ok(mut body) => {
                    if status.is_success() {
                        if let (Some(domain), Some(results)) =
                            (domain_for_links, body["results"].as_array_mut())
                        {
                            for r in results.iter_mut() {
                                if let Some(webui) = r["_links"]["webui"].as_str() {
                                    r["_links"]["webui"] = json!(format!("https://{domain}/wiki{webui}"));
                                }
                            }
                        }
                        Json(body).into_response()
                    } else {
                        (
                            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                            Json(body),
                        )
                            .into_response()
                    }
                }
                Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
            }
        }
    }
}

// ── Shared response helper ────────────────────────────────────────────────────

async fn atlassian_json_response(
    result: Result<reqwest::Response, reqwest::Error>,
) -> axum::response::Response {
    match result {
        Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
        Ok(resp) => {
            let status = resp.status();
            match resp.json::<Value>().await {
                Ok(body) => {
                    if status.is_success() {
                        Json(body).into_response()
                    } else {
                        (
                            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                            Json(body),
                        )
                            .into_response()
                    }
                }
                Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
            }
        }
    }
}
