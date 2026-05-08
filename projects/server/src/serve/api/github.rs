use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use serde_json::Value;
use utoipa::ToSchema;

use super::prelude::*;

fn github_client() -> anyhow::Result<(reqwest::Client, String)> {
    let token = orca_commands::oauth::load_github_token()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("no GitHub token — run `orca login github` or set GITHUB_TOKEN")
        })?;
    let client = reqwest::Client::builder().user_agent("orca/1.0").build()?;
    Ok((client, token))
}

async fn github_get(path: &str, query: &[(&str, &str)]) -> axum::response::Response {
    let (client, token) = match github_client() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let url = format!("https://api.github.com{path}");
    match client
        .get(&url)
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .query(query)
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
                        (
                            StatusCode::from_u16(status.as_u16())
                                .unwrap_or(StatusCode::BAD_GATEWAY),
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

// ── GET /api/github/user ──────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/github/user",
    operation_id = "getGithubUser",
    responses(
        (status = 200, description = "Authenticated GitHub user", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
    ),
    tag = "github"
)]
pub async fn github_user_handler() -> impl IntoResponse {
    github_get("/user", &[]).await
}

// ── GET /api/github/repos ─────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct GithubReposQuery {
    /// Filter: all (default), owner, public, private, member
    #[serde(rename = "type")]
    pub repo_type: Option<String>,
    /// Sort: created, updated, pushed, full_name (default: updated)
    pub sort: Option<String>,
    pub per_page: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/github/repos",
    operation_id = "listGithubRepos",
    params(
        ("type" = Option<String>, Query, description = "Filter type: all | owner | public | private | member"),
        ("sort" = Option<String>, Query, description = "Sort by: created | updated | pushed | full_name"),
        ("per_page" = Option<u32>, Query, description = "Results per page (max 100, default 30)"),
    ),
    responses(
        (status = 200, description = "List of repos for the authenticated user", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
    ),
    tag = "github"
)]
pub async fn github_repos_handler(Query(q): Query<GithubReposQuery>) -> impl IntoResponse {
    let repo_type = q.repo_type.as_deref().unwrap_or("all");
    let sort = q.sort.as_deref().unwrap_or("updated");
    let per_page = q.per_page.unwrap_or(30).to_string();
    github_get(
        "/user/repos",
        &[("type", repo_type), ("sort", sort), ("per_page", &per_page)],
    )
    .await
}

// ── GET /api/github/repos/:owner/:repo/pulls ──────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub struct GithubPrsQuery {
    /// open | closed | all (default: open)
    pub state: Option<String>,
    pub per_page: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/github/repos/{owner}/{repo}/pulls",
    operation_id = "listGithubPRs",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo"  = String, Path, description = "Repository name"),
        ("state" = Option<String>, Query, description = "PR state: open | closed | all"),
        ("per_page" = Option<u32>, Query, description = "Results per page"),
    ),
    responses(
        (status = 200, description = "Pull requests", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
    ),
    tag = "github"
)]
pub async fn github_prs_handler(
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<GithubPrsQuery>,
) -> impl IntoResponse {
    let state = q.state.as_deref().unwrap_or("open");
    let per_page = q.per_page.unwrap_or(30).to_string();
    github_get(
        &format!("/repos/{owner}/{repo}/pulls"),
        &[("state", state), ("per_page", &per_page)],
    )
    .await
}

// ── GET /api/github/repos/:owner/:repo/issues ─────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/github/repos/{owner}/{repo}/issues",
    operation_id = "listGithubIssues",
    params(
        ("owner" = String, Path, description = "Repository owner"),
        ("repo"  = String, Path, description = "Repository name"),
        ("state" = Option<String>, Query, description = "Issue state: open | closed | all"),
        ("per_page" = Option<u32>, Query, description = "Results per page"),
    ),
    responses(
        (status = 200, description = "Issues (PRs excluded)", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
    ),
    tag = "github"
)]
pub async fn github_issues_handler(
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<GithubPrsQuery>,
) -> impl IntoResponse {
    let state = q.state.as_deref().unwrap_or("open");
    let per_page = q.per_page.unwrap_or(30).to_string();
    // filter=all excludes PRs from the issues endpoint
    github_get(
        &format!("/repos/{owner}/{repo}/issues"),
        &[("state", state), ("per_page", &per_page), ("filter", "all")],
    )
    .await
}

// ── GET /api/github/orgs ──────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/github/orgs",
    operation_id = "listGithubOrgs",
    responses(
        (status = 200, description = "Organizations for the authenticated user", body = serde_json::Value),
        (status = 500, body = ErrorResponse),
    ),
    tag = "github"
)]
pub async fn github_orgs_handler() -> impl IntoResponse {
    github_get("/user/orgs", &[("per_page", "100")]).await
}
