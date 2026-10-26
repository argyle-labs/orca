use std::collections::BTreeMap;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde::Deserialize;

use crate::plugin_manager::CatalogEntry;
use crate::release_targets::{TARGET_TRIPLES, linux_asset_candidates};
use crate::update;

pub const PLUGIN_ORG: &str = "argyle-labs";
pub const PLUGIN_TOPIC: &str = "plugin";
const GITHUB_API_VERSION: &str = "2022-11-28";
const PAGE_SIZE: usize = 50;
const RELEASES_PER_REPO: usize = 10;
const MAX_PAGES: usize = 20;
const RELEASE_CONCURRENCY: usize = 8;
const DECLARED_NAME_TOPIC_PREFIX: &str = "plugin-";

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

pub fn is_allowed_plugin_id(name: &str) -> bool {
    plugin_loader::is_valid_plugin_id(name) && !plugin_loader::is_reserved_plugin_id(name)
}

pub fn repo_url_for(full_name: &str) -> String {
    format!("https://github.com/{full_name}")
}

pub fn synthesized_entry(name: &str) -> Option<CatalogEntry> {
    if !is_allowed_plugin_id(name) {
        return None;
    }
    let repo_url = repo_url_for(&format!("{PLUGIN_ORG}/{name}"));
    Some(CatalogEntry {
        name: name.to_string(),
        target_software: name.to_string(),
        docs_url: format!("{repo_url}#readme"),
        repo_url,
        status: "unreleased".to_string(),
    })
}

fn declared_names(repo: &OrgRepo) -> impl Iterator<Item = &str> {
    repo.topics
        .iter()
        .filter_map(|t| t.strip_prefix(DECLARED_NAME_TOPIC_PREFIX))
}

fn repo_publishes(repo: &OrgRepo, name: &str) -> bool {
    name == repo.name || declared_names(repo).any(|d| d == name)
}

fn plugin_name_of(asset: &str, version: &str) -> Option<(String, String)> {
    TARGET_TRIPLES.iter().find_map(|triple| {
        let stem = asset.strip_suffix(&format!("-{triple}"))?;
        let name = stem.strip_suffix(&format!("-v{version}"))?;
        is_allowed_plugin_id(name).then(|| (name.to_string(), triple.to_string()))
    })
}

fn docs_url_of(repo: &OrgRepo, repo_url: &str) -> String {
    [&repo.website, &repo.homepage]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .find(|s| url::Url::parse(s).is_ok_and(|u| u.scheme() == "https" && u.host().is_some()))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{repo_url}#readme"))
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
            if !repo_publishes(repo, &name) {
                tracing::warn!(repo = %repo.full_name, plugin = %name, "release asset names a plugin the repo does not declare; ignoring");
                continue;
            }
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
    if status.is_empty() && is_allowed_plugin_id(&repo.name) {
        status.insert(repo.name.clone(), "unreleased");
    }
    let repo_url = repo_url_for(&repo.full_name);
    let docs_url = docs_url_of(repo, &repo_url);
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

fn owns_name(entry: &CatalogEntry) -> bool {
    entry
        .repo_url
        .rsplit('/')
        .next()
        .is_some_and(|repo| repo == entry.name)
}

pub fn dedupe_entries(entries: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    let mut by_name: BTreeMap<String, Vec<CatalogEntry>> = BTreeMap::new();
    for entry in entries {
        by_name.entry(entry.name.clone()).or_default().push(entry);
    }
    by_name
        .into_iter()
        .filter_map(|(name, mut claimants)| {
            if claimants.len() == 1 {
                return claimants.pop();
            }
            let repos: Vec<&str> = claimants.iter().map(|e| e.repo_url.as_str()).collect();
            match claimants.iter().position(owns_name) {
                Some(i) => {
                    tracing::warn!(plugin = %name, kept = %claimants[i].repo_url, claimants = ?repos, "plugin name published by more than one repo");
                    Some(claimants.swap_remove(i))
                }
                None => {
                    tracing::warn!(plugin = %name, claimants = ?repos, "plugin name claimed by several repos and none is named after it; dropping all");
                    None
                }
            }
        })
        .collect()
}

async fn repo_entries(
    req: utils::http::RequestBuilder,
    repo: OrgRepo,
    triple: String,
    previous: Vec<CatalogEntry>,
) -> (Vec<CatalogEntry>, bool) {
    let releases: Result<Vec<DiscoveredRelease>> = async {
        let resp = req.send().await?;
        if !(200..300).contains(&resp.status) {
            anyhow::bail!("HTTP {}", resp.status);
        }
        Ok(resp.json()?)
    }
    .await;
    match releases {
        Ok(releases) => (entries_for_repo(&repo, &releases, &triple), true),
        Err(e) => {
            tracing::debug!(repo = %repo.full_name, error = %format!("{e:#}"), "plugin release lookup failed; keeping previous entries");
            (previous, false)
        }
    }
}

pub struct Discovery {
    pub entries: Vec<CatalogEntry>,
    pub complete: bool,
}

pub async fn discover(previous: &[CatalogEntry]) -> Result<Discovery> {
    let token = if update::source_is_github() {
        update::resolve_github_token()
    } else {
        String::new()
    };
    discover_from(
        &update::release_host_api_base(),
        &token,
        update::build_target(),
        previous,
    )
    .await
}

pub async fn discover_from(
    host_api_base: &str,
    token: &str,
    triple: &str,
    previous: &[CatalogEntry],
) -> Result<Discovery> {
    let client = utils::http::Client::new();
    let ua = format!("orca/{}", crate::build_info::version());
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
    let mut first_of_previous_page: Option<String> = None;
    for page in 1..=MAX_PAGES {
        let resp = get(format!(
            "{host_api_base}/orgs/{PLUGIN_ORG}/repos?limit={PAGE_SIZE}&per_page={PAGE_SIZE}&page={page}"
        ))
        .send()
        .await
        .with_context(|| format!("list {PLUGIN_ORG} repos from {host_api_base}"))?;
        if !(200..300).contains(&resp.status) {
            anyhow::bail!(
                "list {PLUGIN_ORG} repos from {host_api_base}: HTTP {}",
                resp.status
            );
        }
        let batch: Vec<OrgRepo> = resp.json().context("parse org repos json")?;
        let first = batch.first().map(|r| r.full_name.clone());
        if first.is_some() && first == first_of_previous_page {
            break;
        }
        first_of_previous_page = first;
        let done = batch.len() < PAGE_SIZE;
        repos.extend(batch);
        if done {
            break;
        }
    }
    repos.retain(is_plugin_repo);
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    repos.dedup_by(|a, b| a.full_name == b.full_name);

    let lookups: Vec<_> = repos
        .into_iter()
        .map(|repo| {
            let req = get(format!(
                "{host_api_base}/repos/{}/releases?limit={RELEASES_PER_REPO}&per_page={RELEASES_PER_REPO}",
                repo.full_name
            ));
            let repo_url = repo_url_for(&repo.full_name);
            let kept = previous
                .iter()
                .filter(|e| e.repo_url == repo_url)
                .cloned()
                .collect();
            repo_entries(req, repo, triple.to_string(), kept)
        })
        .collect();
    let results: Vec<(Vec<CatalogEntry>, bool)> = futures::stream::iter(lookups)
        .buffer_unordered(RELEASE_CONCURRENCY)
        .collect()
        .await;
    let complete = results.iter().all(|(_, ok)| *ok);
    let entries = dedupe_entries(results.into_iter().flat_map(|(e, _)| e).collect());
    Ok(Discovery { entries, complete })
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
        let got = entries_for_repo(
            &repo("arr", &["plugin", "plugin-sonarr", "plugin-radarr"]),
            &[rel],
            TRIPLE,
        );
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

        let got = discover_from(&server.uri(), "", TRIPLE, &[]).await.unwrap();
        assert!(got.complete);
        let rows: Vec<_> = got
            .entries
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
        assert!(discover_from(&server.uri(), "", TRIPLE, &[]).await.is_err());
    }

    #[test]
    fn a_repo_cannot_publish_an_undeclared_plugin_name() {
        let rel = release(
            "v0.1.0",
            false,
            &[
                "evil-v0.1.0-x86_64-unknown-linux-musl",
                "jellyfin-v0.1.0-x86_64-unknown-linux-musl",
            ],
        );
        let got = entries_for_repo(&repo("evil", &["plugin"]), &[rel], TRIPLE);
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["evil"]);
    }

    #[test]
    fn duplicate_names_prefer_the_repo_named_after_the_plugin() {
        let mut a = synthesized_entry("sonarr").unwrap();
        a.repo_url = "https://github.com/argyle-labs/arr".to_string();
        let b = synthesized_entry("sonarr").unwrap();
        let got = dedupe_entries(vec![a.clone(), b.clone()]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].repo_url, b.repo_url);
        let got = dedupe_entries(vec![b.clone(), a]);
        assert_eq!(got[0].repo_url, b.repo_url);
    }

    #[test]
    fn duplicate_names_without_an_owner_are_dropped() {
        let mut a = synthesized_entry("sonarr").unwrap();
        a.repo_url = "https://github.com/argyle-labs/arr".to_string();
        let mut b = synthesized_entry("sonarr").unwrap();
        b.repo_url = "https://github.com/argyle-labs/evil".to_string();
        assert!(dedupe_entries(vec![a.clone(), b]).is_empty());
        let got = dedupe_entries(vec![a.clone()]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].repo_url, a.repo_url);
    }

    #[test]
    fn plugin_names_are_validated() {
        for ok in ["nut", "a", "x0", "arr-sonarr", &"a".repeat(64)] {
            assert!(is_allowed_plugin_id(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "0x",
            "x-",
            "a--b",
            "ui",
            "github_token",
            "Nut",
            "a_b",
            "a/b",
            "../x",
            "a.b",
            &"a".repeat(65),
        ] {
            assert!(!is_allowed_plugin_id(bad), "{bad}");
        }
        assert!(plugin_name_of("../x-v0.1.0-x86_64-unknown-linux-musl", "0.1.0").is_none());
        assert!(synthesized_entry("Bad/Name").is_none());
        assert!(synthesized_entry("ui").is_none());
    }

    #[test]
    fn a_synthesized_entry_points_at_the_org_repo() {
        let e = synthesized_entry("nut").unwrap();
        assert_eq!(e.repo_url, "https://github.com/argyle-labs/nut");
        assert_eq!(e.target_software, "nut");
    }

    #[test]
    fn a_non_https_website_falls_back_to_the_readme() {
        let mut r = repo("nut", &["plugin"]);
        r.website = Some("javascript:alert(1)".to_string());
        r.homepage = Some("http://docs.example/nut".to_string());
        let got = entries_for_repo(&r, &[], TRIPLE);
        assert_eq!(got[0].docs_url, "https://github.com/argyle-labs/nut#readme");
    }

    #[tokio::test]
    async fn a_failed_release_lookup_keeps_previous_entries_and_marks_partial() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/orgs/{PLUGIN_ORG}/repos")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "nut", "full_name": "argyle-labs/nut", "topics": ["plugin"]},
                {"name": "adguard", "full_name": "argyle-labs/adguard", "topics": ["plugin"]}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/argyle-labs/nut/releases"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/argyle-labs/adguard/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let mut prev = synthesized_entry("nut").unwrap();
        prev.status = "available".to_string();
        let got = discover_from(&server.uri(), "", TRIPLE, &[prev])
            .await
            .unwrap();
        assert!(!got.complete);
        let rows: Vec<_> = got
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.status.as_str()))
            .collect();
        assert_eq!(rows, vec![("adguard", "unreleased"), ("nut", "available")]);
    }

    #[tokio::test]
    async fn pagination_stops_when_a_page_repeats() {
        let server = MockServer::start().await;
        let page: Vec<_> = (0..PAGE_SIZE)
            .map(|i| serde_json::json!({"name": format!("r{i}"), "full_name": format!("argyle-labs/r{i}"), "topics": []}))
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("/orgs/{PLUGIN_ORG}/repos")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(page)))
            .expect(2)
            .mount(&server)
            .await;
        let got = discover_from(&server.uri(), "", TRIPLE, &[]).await.unwrap();
        assert!(got.entries.is_empty());
    }

    #[tokio::test]
    async fn pagination_is_capped() {
        let server = MockServer::start().await;
        for p in 1..=(MAX_PAGES + 1) {
            let page: Vec<_> = (0..PAGE_SIZE)
                .map(|i| serde_json::json!({"name": format!("p{p}r{i}"), "full_name": format!("argyle-labs/p{p}r{i}"), "topics": []}))
                .collect();
            Mock::given(method("GET"))
                .and(path(format!("/orgs/{PLUGIN_ORG}/repos")))
                .and(query_param("page", p.to_string()))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::Value::Array(page)),
                )
                .mount(&server)
                .await;
        }
        discover_from(&server.uri(), "", TRIPLE, &[]).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), MAX_PAGES);
    }
}
