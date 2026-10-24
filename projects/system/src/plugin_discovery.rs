use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::plugin_manager::CatalogEntry;
use crate::release_targets::{TARGET_TRIPLES, linux_asset_candidates};
use crate::update;

pub const PLUGIN_ORG: &str = "argyle-labs";
pub const PLUGIN_TOPIC: &str = "plugin";
const GITHUB_API_VERSION: &str = "2022-11-28";
const ORCA_VERSION: &str = env!("ORCA_VERSION");
const PAGE_SIZE: usize = 50;
const RELEASES_PER_REPO: usize = 10;

#[derive(Deserialize, Debug, Clone)]
pub struct OrgRepo {
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DiscoveredRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<DiscoveredAsset>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DiscoveredAsset {
    pub name: String,
}

pub fn is_plugin_repo(repo: &OrgRepo) -> bool {
    !repo.archived && repo.topics.iter().any(|t| t == PLUGIN_TOPIC)
}

fn plugin_name_of(asset: &str, version: &str) -> Option<(String, String)> {
    TARGET_TRIPLES.iter().find_map(|triple| {
        let stem = asset.strip_suffix(&format!("-{triple}"))?;
        let name = stem.strip_suffix(&format!("-v{version}"))?;
        (!name.is_empty()).then(|| (name.to_string(), triple.to_string()))
    })
}

pub fn entries_for_repo(
    repo: &OrgRepo,
    releases: &[DiscoveredRelease],
    triple: &str,
) -> Vec<CatalogEntry> {
    let candidates = linux_asset_candidates(triple);
    let mut status: BTreeMap<String, &'static str> = BTreeMap::new();
    for release in releases {
        let version = release.tag_name.trim_start_matches('v');
        for asset in &release.assets {
            let Some((name, asset_triple)) = plugin_name_of(&asset.name, version) else {
                continue;
            };
            let found = if !candidates.contains(&asset_triple) {
                "unreleased"
            } else if release.prerelease {
                "prerelease"
            } else {
                "available"
            };
            let slot = status.entry(name).or_insert(found);
            if rank(found) > rank(slot) {
                *slot = found;
            }
        }
    }
    if status.is_empty() {
        status.insert(repo.name.clone(), "unreleased");
    }
    let repo_url = format!("https://github.com/{}", repo.full_name);
    let docs_url = [&repo.website, &repo.homepage]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{repo_url}#readme"));
    status
        .into_iter()
        .map(|(name, status)| CatalogEntry {
            target_software: name.clone(),
            name,
            repo_url: repo_url.clone(),
            docs_url: docs_url.clone(),
            status: status.to_string(),
        })
        .collect()
}

fn rank(status: &str) -> u8 {
    match status {
        "available" => 2,
        "prerelease" => 1,
        _ => 0,
    }
}

pub async fn discover() -> Result<Vec<CatalogEntry>> {
    let token = if update::source_is_github() {
        update::resolve_github_token()
    } else {
        String::new()
    };
    discover_from(
        &update::release_host_api_base(),
        &token,
        update::build_target(),
    )
    .await
}

pub async fn discover_from(
    host_api_base: &str,
    token: &str,
    triple: &str,
) -> Result<Vec<CatalogEntry>> {
    let client = utils::http::Client::new();
    let ua = format!("orca/{ORCA_VERSION}");
    let get = |url: String| {
        let mut r = client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("User-Agent", &ua)
            .timeout(std::time::Duration::from_secs(15));
        if !token.is_empty() {
            r = r.bearer(token);
        }
        r
    };

    let mut repos: Vec<OrgRepo> = Vec::new();
    for page in 1.. {
        let batch: Vec<OrgRepo> = get(format!(
            "{host_api_base}/orgs/{PLUGIN_ORG}/repos?limit={PAGE_SIZE}&per_page={PAGE_SIZE}&page={page}"
        ))
        .send()
        .await
        .with_context(|| format!("list {PLUGIN_ORG} repos from {host_api_base}"))?
        .json()
        .context("parse org repos json")?;
        let done = batch.len() < PAGE_SIZE;
        repos.extend(batch);
        if done {
            break;
        }
    }
    repos.retain(is_plugin_repo);
    repos.sort_by(|a, b| a.name.cmp(&b.name));

    let lookups = repos.iter().map(|repo| {
        let req = get(format!(
            "{host_api_base}/repos/{}/releases?limit={RELEASES_PER_REPO}&per_page={RELEASES_PER_REPO}",
            repo.full_name
        ));
        async move {
            let releases: Vec<DiscoveredRelease> = match req.send().await {
                Ok(resp) => resp.json().unwrap_or_default(),
                Err(e) => {
                    tracing::debug!(repo = %repo.full_name, error = %e, "plugin release lookup failed");
                    Vec::new()
                }
            };
            entries_for_repo(repo, &releases, triple)
        }
    });
    Ok(futures::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TRIPLE: &str = "x86_64-unknown-linux-musl";

    fn repo(name: &str, topics: &[&str]) -> OrgRepo {
        OrgRepo {
            name: name.to_string(),
            full_name: format!("{PLUGIN_ORG}/{name}"),
            topics: topics.iter().map(|t| t.to_string()).collect(),
            archived: false,
            website: None,
            homepage: None,
        }
    }

    fn release(tag: &str, prerelease: bool, assets: &[&str]) -> DiscoveredRelease {
        DiscoveredRelease {
            tag_name: tag.to_string(),
            prerelease,
            assets: assets
                .iter()
                .map(|a| DiscoveredAsset {
                    name: a.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn only_unarchived_repos_with_the_topic_are_plugins() {
        assert!(is_plugin_repo(&repo("nut", &["monitoring", "plugin"])));
        assert!(!is_plugin_repo(&repo("walrus", &[])));
        let mut archived = repo("kavita", &["plugin"]);
        archived.archived = true;
        assert!(!is_plugin_repo(&archived));
    }

    #[test]
    fn a_repo_without_releases_is_unreleased_under_its_own_name() {
        let got = entries_for_repo(&repo("adguard", &["plugin"]), &[], TRIPLE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "adguard");
        assert_eq!(got[0].target_software, "adguard");
        assert_eq!(got[0].status, "unreleased");
        assert_eq!(got[0].repo_url, "https://github.com/argyle-labs/adguard");
        assert_eq!(
            got[0].docs_url,
            "https://github.com/argyle-labs/adguard#readme"
        );
    }

    #[test]
    fn a_stable_release_with_a_matching_asset_is_available() {
        let rel = release(
            "v0.1.0",
            false,
            &[
                "nut-v0.1.0-x86_64-unknown-linux-musl",
                "nut-v0.1.0-x86_64-unknown-linux-musl.sha256",
            ],
        );
        let got = entries_for_repo(&repo("nut", &["plugin"]), &[rel], TRIPLE);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].status, "available");
    }

    #[test]
    fn a_prerelease_only_match_is_prerelease() {
        let rel = release(
            "v0.0.1-rc.1",
            true,
            &["garage-v0.0.1-rc.1-x86_64-unknown-linux-musl"],
        );
        let got = entries_for_repo(&repo("garage", &["plugin"]), &[rel], TRIPLE);
        assert_eq!(got[0].status, "prerelease");
    }

    #[test]
    fn assets_only_for_other_platforms_are_unreleased() {
        let rel = release("v0.1.0", false, &["nut-v0.1.0-aarch64-apple-darwin"]);
        let got = entries_for_repo(&repo("nut", &["plugin"]), &[rel], TRIPLE);
        assert_eq!(got[0].name, "nut");
        assert_eq!(got[0].status, "unreleased");
    }

    #[test]
    fn a_workspace_repo_yields_one_entry_per_published_binary() {
        let rel = release(
            "v0.2.0",
            false,
            &[
                "sonarr-v0.2.0-x86_64-unknown-linux-musl",
                "radarr-v0.2.0-x86_64-unknown-linux-musl",
            ],
        );
        let got = entries_for_repo(&repo("arr", &["plugin"]), &[rel], TRIPLE);
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["radarr", "sonarr"]);
        assert!(
            got.iter()
                .all(|e| e.repo_url == "https://github.com/argyle-labs/arr")
        );
    }

    #[test]
    fn the_repo_website_is_the_docs_url() {
        let mut r = repo("nut", &["plugin"]);
        r.website = Some("https://docs.example/nut".to_string());
        let got = entries_for_repo(&r, &[], TRIPLE);
        assert_eq!(got[0].docs_url, "https://docs.example/nut");
    }

    #[tokio::test]
    async fn discovery_reads_the_org_and_each_plugin_repos_releases() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/orgs/{PLUGIN_ORG}/repos")))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "nut", "full_name": "argyle-labs/nut", "topics": ["plugin"]},
                {"name": "walrus", "full_name": "argyle-labs/walrus", "topics": []},
                {"name": "adguard", "full_name": "argyle-labs/adguard", "topics": ["plugin"]}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/argyle-labs/nut/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"tag_name": "v0.1.0", "prerelease": false,
                 "assets": [{"name": "nut-v0.1.0-x86_64-unknown-linux-musl"}]}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/argyle-labs/adguard/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let got = discover_from(&server.uri(), "", TRIPLE).await.unwrap();
        let rows: Vec<_> = got
            .iter()
            .map(|e| (e.name.as_str(), e.status.as_str()))
            .collect();
        assert_eq!(rows, vec![("adguard", "unreleased"), ("nut", "available")]);
    }

    #[tokio::test]
    async fn discovery_fails_when_the_org_listing_fails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/orgs/{PLUGIN_ORG}/repos")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(discover_from(&server.uri(), "", TRIPLE).await.is_err());
    }
}
