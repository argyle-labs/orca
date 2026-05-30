//! OpenAPI spec registry — listing on-disk + db-backed + plugin-declared specs,
//! fetching and refreshing URL-registered specs, removing rows.
#![allow(clippy::disallowed_types)] // OpenAPI specs are arbitrary upstream JSON — escape hatch.

use super::registry::specs_dir;
use super::{DbSpecRow, RegisterSpecResult, SpecFilesPresence, SpecMetaRow};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;

fn validate_repo(repo: &str) -> bool {
    !repo.is_empty()
        && repo
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub async fn list_specs() -> Result<Vec<SpecMetaRow>> {
    let dir = specs_dir();

    let registry: Vec<Value> = match std::fs::read_to_string(dir.join("registry.json")) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let mut by_repo: std::collections::HashMap<String, Value> = registry
        .into_iter()
        .filter_map(|e| {
            let repo = e.get("repo")?.as_str()?.to_string();
            Some((repo, e))
        })
        .collect();

    let mut out: Vec<SpecMetaRow> = Vec::new();

    if let Ok(read) = std::fs::read_dir(&dir) {
        let mut repos: Vec<String> = read
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "registry.json" {
                    return None;
                }
                if let Some(stem) = name.strip_suffix(".json") {
                    if stem.ends_with(".public") {
                        return None;
                    }
                    return Some(stem.to_string());
                }
                if let Some(stem) = name.strip_suffix(".graphql") {
                    return Some(stem.to_string());
                }
                None
            })
            .collect();
        repos.sort();
        repos.dedup();

        for repo in repos {
            let entry = by_repo.remove(&repo);
            let project = entry
                .as_ref()
                .and_then(|v| v.get("project"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| repo.clone());
            let base_url = entry
                .as_ref()
                .and_then(|v| v.get("baseUrl"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let source = entry
                .as_ref()
                .and_then(|v| v.get("source"))
                .and_then(|v| v.as_str())
                .unwrap_or("manual")
                .to_string();
            let has_full = dir.join(format!("{repo}.json")).exists();
            let has_public = dir.join(format!("{repo}.public.json")).exists();
            let has_graphql = dir.join(format!("{repo}.graphql")).exists();
            out.push(SpecMetaRow {
                repo,
                project,
                source,
                namespace: "orca".to_string(),
                source_mcp: None,
                base_url,
                captured_at: None,
                path_count: None,
                has_graphql,
                files: SpecFilesPresence {
                    full: has_full,
                    public: has_public,
                },
            });
        }
    }

    if let Ok(conn) = db::open_default() {
        if let Ok(db_specs) = db::openapi_specs::list(&conn) {
            let disk_names: std::collections::HashSet<String> =
                out.iter().map(|r| r.repo.clone()).collect();
            for s in db_specs {
                if disk_names.contains(&s.name) {
                    continue;
                }
                let path_count = s
                    .spec_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|v| v["paths"].as_object().map(|p| p.len() as u32));
                let namespace = s.source_mcp.clone().unwrap_or_else(|| "orca".to_string());
                let source = if s.source_mcp.is_some() { "mcp" } else { "url" };
                out.push(SpecMetaRow {
                    repo: s.name.clone(),
                    project: s.name,
                    source: source.to_string(),
                    namespace,
                    source_mcp: s.source_mcp,
                    base_url: s.url,
                    captured_at: s.cached_at,
                    path_count,
                    has_graphql: false,
                    files: SpecFilesPresence {
                        full: true,
                        public: false,
                    },
                });
            }
        }

        if let Ok(plugins) = db::plugins::list(&conn) {
            for plugin in plugins
                .iter()
                .filter(|p| p.specs_dir.is_some() && p.enabled)
            {
                let plugin_dir = std::path::PathBuf::from(plugin.specs_dir.as_deref().unwrap());
                let Ok(read) = std::fs::read_dir(&plugin_dir) else {
                    continue;
                };
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut plugin_repos: Vec<String> = read
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if let Some(stem) = name.strip_suffix(".json") {
                            if stem.ends_with(".public") {
                                return None;
                            }
                            return Some(stem.to_string());
                        }
                        if let Some(stem) = name.strip_suffix(".graphql") {
                            return Some(stem.to_string());
                        }
                        None
                    })
                    .collect();
                plugin_repos.sort();
                plugin_repos.dedup();
                for repo in plugin_repos {
                    if !seen.insert(repo.clone()) {
                        continue;
                    }
                    let has_full = plugin_dir.join(format!("{repo}.json")).exists();
                    let has_public = plugin_dir.join(format!("{repo}.public.json")).exists();
                    let has_graphql = plugin_dir.join(format!("{repo}.graphql")).exists();
                    out.push(SpecMetaRow {
                        repo: repo.clone(),
                        project: repo,
                        source: "plugin".to_string(),
                        namespace: plugin.id.clone(),
                        source_mcp: None,
                        base_url: None,
                        captured_at: None,
                        path_count: None,
                        has_graphql,
                        files: SpecFilesPresence {
                            full: has_full,
                            public: has_public,
                        },
                    });
                }
            }
        }
    }

    Ok(out)
}

pub async fn list_db_specs() -> Result<Vec<DbSpecRow>> {
    let conn = db::open_default()?;
    let rows = db::openapi_specs::list(&conn)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let path_count = r
                .spec_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .and_then(|v| v["paths"].as_object().map(|p| p.len() as u32));
            DbSpecRow {
                name: r.name,
                url: r.url,
                source_mcp: r.source_mcp,
                path_count,
                cached_at: r.cached_at,
                enabled: r.enabled,
            }
        })
        .collect())
}

pub async fn register_spec(name: &str, url: &str) -> Result<RegisterSpecResult> {
    if name.is_empty() || url.is_empty() {
        return Err(anyhow!("name and url are required"));
    }
    let resp = reqwest::get(url)
        .await
        .with_context(|| format!("fetch {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} fetching {url}", resp.status()));
    }
    let spec_json: Value = resp.json().await.context("invalid JSON from spec URL")?;
    let spec_text = serde_json::to_string(&spec_json)?;
    let path_count = spec_json["paths"].as_object().map(|p| p.len() as u32);
    let cached_at = chrono::Utc::now().to_rfc3339();
    let conn = db::open_default()?;
    let row = db::openapi_specs::OpenApiSpecRow {
        name: name.to_string(),
        url: Some(url.to_string()),
        source_mcp: None,
        spec_json: Some(spec_text),
        cached_at: Some(cached_at.clone()),
        enabled: true,
    };
    db::openapi_specs::upsert(&conn, &row)?;
    Ok(RegisterSpecResult {
        name: name.to_string(),
        url: Some(url.to_string()),
        source_mcp: None,
        path_count,
        cached_at: Some(cached_at),
        enabled: true,
    })
}

pub async fn refresh_spec(name: &str) -> Result<RegisterSpecResult> {
    if !validate_repo(name) {
        return Err(anyhow!("invalid spec name"));
    }
    let conn = db::open_default()?;
    let row =
        db::openapi_specs::get(&conn, name)?.ok_or_else(|| anyhow!("no spec named '{name}'"))?;
    let url = row
        .url
        .clone()
        .ok_or_else(|| anyhow!("spec '{name}' has no URL — cannot refresh"))?;
    let resp = reqwest::get(&url)
        .await
        .with_context(|| format!("fetch {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP {} fetching {url}", resp.status()));
    }
    let spec_json: Value = resp.json().await.context("invalid JSON from spec URL")?;
    let spec_text = serde_json::to_string(&spec_json)?;
    let path_count = spec_json["paths"].as_object().map(|p| p.len() as u32);
    let cached_at = chrono::Utc::now().to_rfc3339();
    let updated = db::openapi_specs::OpenApiSpecRow {
        name: row.name.clone(),
        url: row.url.clone(),
        source_mcp: row.source_mcp.clone(),
        spec_json: Some(spec_text),
        cached_at: Some(cached_at.clone()),
        enabled: row.enabled,
    };
    db::openapi_specs::upsert(&conn, &updated)?;
    Ok(RegisterSpecResult {
        name: row.name,
        url: row.url,
        source_mcp: row.source_mcp,
        path_count,
        cached_at: Some(cached_at),
        enabled: row.enabled,
    })
}

pub async fn unregister_spec(name: &str) -> Result<bool> {
    if !validate_repo(name) {
        return Err(anyhow!("invalid spec name"));
    }
    let conn = db::open_default()?;
    db::openapi_specs::remove(&conn, name)
}
