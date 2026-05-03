use axum::{
    extract::Query,
    response::{IntoResponse, Json},
};
use orca_utils::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

#[allow(unused_imports)]
use super::prelude::*;

#[derive(Serialize, ToSchema)]
pub struct RepoInfo {
    pub workspace: String,
    pub slug: String,
    pub remote: String,
}

#[derive(Deserialize, ToSchema)]
pub struct PrQuery {
    pub workspace: String,
    pub slug: String,
}

#[utoipa::path(
    get,
    path = "/api/bitbucket/repos",
    operation_id = "listBitbucketRepos",
    responses(
        (status = 200, description = "Bitbucket repos found under REBUY_ROOT", body = Vec<RepoInfo>),
    ),
    tag = "bitbucket"
)]
/// GET /api/bitbucket/repos
/// Scans REBUY_ROOT (or ~/code/rebuy) for git dirs with Bitbucket remotes.
pub async fn repos_handler() -> impl IntoResponse {
    let repos = scan_bitbucket_repos();
    Json(repos)
}

#[utoipa::path(
    get,
    path = "/api/bitbucket/prs",
    operation_id = "listBitbucketPRs",
    params(
        ("workspace" = String, Query, description = "Bitbucket workspace slug"),
        ("slug" = String, Query, description = "Repository slug"),
    ),
    responses(
        (status = 200, description = "Open pull requests from Bitbucket API (pagelen=50)", body = serde_json::Value),
        (status = 500, description = "Credential or upstream error", body = ErrorResponse),
    ),
    tag = "bitbucket"
)]
/// GET /api/bitbucket/prs?workspace=X&slug=Y
/// Proxies to the Bitbucket REST API using stored Atlassian credentials.
pub async fn prs_handler(Query(q): Query<PrQuery>) -> impl IntoResponse {
    match fetch_prs(&q.workspace, &q.slug).await {
        Ok(prs) => Json(prs).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

fn scan_bitbucket_repos() -> Vec<RepoInfo> {
    let home = dirs::home_dir().unwrap_or_default();
    let rebuy_root = std::env::var("REBUY_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| home.join("code/rebuy"));

    let Ok(entries) = std::fs::read_dir(&rebuy_root) else {
        return vec![];
    };

    let mut repos = vec![];
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(output) = std::process::Command::new("git")
            .args(["-C", &path.to_string_lossy(), "remote", "get-url", "origin"])
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if let Some(caps) = parse_bitbucket_remote(&remote) {
            repos.push(RepoInfo {
                workspace: caps.0,
                slug: caps.1,
                remote,
            });
        }
    }
    repos
}

fn parse_bitbucket_remote(remote: &str) -> Option<(String, String)> {
    // Handles:
    //   git@bitbucket.org:workspace/slug.git
    //   https://bitbucket.org/workspace/slug.git
    let re = regex::Regex::new(r"bitbucket\.org[:/]([^/]+)/([^/.]+)").ok()?;
    let caps = re.captures(remote)?;
    Some((caps[1].to_string(), caps[2].to_string()))
}

async fn fetch_prs(workspace: &str, slug: &str) -> anyhow::Result<serde_json::Value> {
    let config = Config::load()?;
    let (_domain, email, token) = super::atlassian::atlassian_creds(&config)?;

    let url = format!(
        "https://api.bitbucket.org/2.0/repositories/{workspace}/{slug}/pullrequests?state=OPEN&pagelen=50"
    );

    let resp = reqwest::Client::new()
        .get(&url)
        .basic_auth(&email, Some(&token))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Bitbucket API {status}: {body}");
    }

    Ok(resp.json().await?)
}

