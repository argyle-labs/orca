//! OpenAPI spec registry — higher-level operations that compose the raw
//! [`openapi_specs`](crate::openapi_specs) CRUD with on-disk spec scanning
//! and HTTP fetch. This is the db-side sync primitive for OpenAPI specs.
//!
//! Shared row shapes used by the namespace-level `#[orca_tool]` sites also
//! live here — they describe rows owned by this crate.
//!
//! The scaffold builders and the public-spec filter construct dynamic
//! OpenAPI documents, which justifies the scoped `serde_json::Value`
//! escape hatch (documented opaque OpenAPI shape).

use anyhow::{Context, Result, anyhow};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::openapi_specs;

// ── Shared row shapes ──────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecFilesPresence {
    pub full: bool,
    pub public: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SpecMetaRow {
    pub repo: String,
    pub project: String,
    /// "manual" | "url" | "mcp" | "plugin"
    pub source: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    pub has_graphql: bool,
    pub files: SpecFilesPresence,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DbSpecRow {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegisterSpecResult {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mcp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_at: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SyncMcpSpecsResult {
    pub server: String,
    pub synced: u32,
    pub errors: Vec<String>,
}

// ── Spec directory + on-disk registry ─────────────────────────────────────

/// Directory holding all tracked external API specs — both OpenAPI (.json)
/// and GraphQL (.graphql) files live here.
pub fn specs_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("ORCA_SPECS_DIR") {
        return PathBuf::from(custom);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".orca/specs")
}

/// Registry entry for a tracked external API spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecEntry {
    pub repo: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// "manual" or "snapshot" (snapshot not yet implemented)
    pub source: String,
    #[serde(rename = "baseUrl", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(rename = "capturedAt", skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

pub struct SpecRegistry {
    pub entries: Vec<SpecEntry>,
}

impl SpecRegistry {
    pub fn load() -> Result<Self> {
        let path = specs_dir().join("registry.json");
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self { entries })
    }

    pub fn save(&self) -> Result<()> {
        let dir = specs_dir();
        std::fs::create_dir_all(&dir)?;
        let raw = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(dir.join("registry.json"), raw)?;
        Ok(())
    }

    /// Register an entry and scaffold both spec files if they don't exist yet.
    /// Returns the path to the full spec file.
    pub fn add(&mut self, entry: SpecEntry) -> Result<PathBuf> {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.repo == entry.repo) {
            *existing = entry.clone();
        } else {
            self.entries.push(entry.clone());
        }
        self.save()?;

        let dir = specs_dir();
        let full_path = dir.join(format!("{}.json", entry.repo));
        let public_path = dir.join(format!("{}.public.json", entry.repo));

        if !full_path.exists() {
            let scaffold = scaffold::full_spec(&entry);
            std::fs::write(&full_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        if !public_path.exists() {
            let scaffold = scaffold::public_spec(&entry);
            std::fs::write(&public_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        Ok(full_path)
    }
}

// ── OpenAPI document builders + public-spec filter ────────────────────────
// Dynamic JSON construction — scoped Value escape hatch.
#[allow(clippy::disallowed_types)]
pub mod scaffold {
    use super::SpecEntry;
    use serde_json::{Value, json};

    fn base_spec_info(entry: &SpecEntry, title_suffix: &str) -> Value {
        let now = utils::time::now_rfc3339();
        let captured = entry.captured_at.as_deref().unwrap_or(&now);
        let servers = entry
            .base_url
            .as_ref()
            .map(|u| json!([{ "url": u, "description": "Production" }]))
            .unwrap_or(json!([]));
        json!({
            "openapi": "3.1.0",
            "info": {
                "title": format!("{}{}", entry.repo, title_suffix),
                "version": "0.0.0",
                "description": entry.description.as_deref().unwrap_or("")
            },
            "x-orca": {
                "repo": entry.repo,
                "project": entry.project,
                "source": entry.source,
                "baseUrl": entry.base_url,
                "capturedAt": captured
            },
            "servers": servers,
            "paths": {},
            "components": { "schemas": {}, "securitySchemes": {} }
        })
    }

    /// Full internal spec scaffold — all endpoints, internal + public.
    pub fn full_spec(entry: &SpecEntry) -> Value {
        let mut spec = base_spec_info(entry, "");
        spec["tags"] = json!([
            { "name": "public",   "description": "Publicly accessible endpoints" },
            { "name": "internal", "description": "Internal endpoints — not for external consumers" }
        ]);
        spec
    }

    /// Standalone public spec scaffold — complete, self-contained, public endpoints only.
    pub fn public_spec(entry: &SpecEntry) -> Value {
        let mut spec = base_spec_info(entry, " (Public API)");
        spec["tags"] = json!([
            { "name": "public", "description": "Publicly accessible endpoints" }
        ]);
        spec
    }
}

// Public-spec filter operates on opaque OpenAPI Value documents.
#[allow(clippy::disallowed_types)]
mod filter {
    use serde_json::Value;

    const METHODS: &[&str] = &[
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];

    /// Domain tags in orca's own spec that are publicly accessible.
    const BRAIN_PUBLIC_DOMAINS: &[&str] = &["docs", "library"];

    fn filter_ops(mut spec: Value, keep: impl Fn(&Value) -> bool) -> Value {
        if let Some(paths) = spec["paths"].as_object_mut() {
            let keys: Vec<String> = paths.keys().cloned().collect();
            for key in &keys {
                if let Some(item) = paths.get_mut(key).and_then(|v| v.as_object_mut()) {
                    for method in METHODS {
                        if let Some(op) = item.get(*method)
                            && !keep(op)
                        {
                            item.remove(*method);
                        }
                    }
                }
            }
            let empty: Vec<String> = paths
                .iter()
                .filter(|(_, v)| !METHODS.iter().any(|m| v.get(m).is_some()))
                .map(|(k, _)| k.clone())
                .collect();
            for p in empty {
                paths.remove(&p);
            }
        }
        spec
    }

    /// Filter orca's own spec to only operations in publicly accessible domain groups.
    pub fn filter_orca_public(spec: Value) -> Value {
        let mut filtered = filter_ops(spec, |op| {
            op["tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .any(|t| BRAIN_PUBLIC_DOMAINS.contains(&t.as_str().unwrap_or("")))
                })
                .unwrap_or(false)
        });

        let used_tags: std::collections::HashSet<String> = filtered["paths"]
            .as_object()
            .into_iter()
            .flat_map(|paths| paths.values())
            .flat_map(|item| METHODS.iter().filter_map(|m| item.get(*m)))
            .flat_map(|op| op["tags"].as_array().into_iter().flatten())
            .filter_map(|t| t.as_str().map(String::from))
            .collect();

        if let Some(tags) = filtered["tags"].as_array() {
            let pruned: Vec<Value> = tags
                .iter()
                .filter(|t| {
                    t["name"]
                        .as_str()
                        .map(|n| used_tags.contains(n))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            filtered["tags"] = Value::Array(pruned);
        }

        filtered
    }
}

pub use filter::filter_orca_public;

// ── Registry-level operations ─────────────────────────────────────────────
// `serde_json::Value` here is legitimate: spec_json blobs are arbitrary
// upstream OpenAPI documents.
#[allow(clippy::disallowed_types)]
mod ops {
    use super::*;
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

        if let Ok(conn) = crate::open_default() {
            if let Ok(db_specs) = openapi_specs::list(&conn) {
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

            if let Ok(plugins) = crate::plugins::list(&conn) {
                for plugin in plugins
                    .iter()
                    .filter(|p| p.specs_dir.is_some() && p.enabled)
                {
                    let plugin_dir = std::path::PathBuf::from(plugin.specs_dir.as_deref().unwrap());
                    let Ok(read) = std::fs::read_dir(&plugin_dir) else {
                        continue;
                    };
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
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
        let conn = crate::open_default()?;
        let rows = openapi_specs::list(&conn)?;
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
        let cached_at = utils::time::now_rfc3339();
        let conn = crate::open_default()?;
        let row = openapi_specs::OpenApiSpecRow {
            name: name.to_string(),
            url: Some(url.to_string()),
            source_mcp: None,
            spec_json: Some(spec_text),
            cached_at: Some(cached_at.clone()),
            enabled: true,
        };
        openapi_specs::upsert(&conn, &row)?;
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
        let conn = crate::open_default()?;
        let row =
            openapi_specs::get(&conn, name)?.ok_or_else(|| anyhow!("no spec named '{name}'"))?;
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
        let cached_at = utils::time::now_rfc3339();
        let updated = openapi_specs::OpenApiSpecRow {
            name: row.name.clone(),
            url: row.url.clone(),
            source_mcp: row.source_mcp.clone(),
            spec_json: Some(spec_text),
            cached_at: Some(cached_at.clone()),
            enabled: row.enabled,
        };
        openapi_specs::upsert(&conn, &updated)?;
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
        let conn = crate::open_default()?;
        openapi_specs::remove(&conn, name)
    }
}

pub use ops::{list_db_specs, list_specs, refresh_spec, register_spec, unregister_spec};
