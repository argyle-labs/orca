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

#[cfg(test)]
#[allow(clippy::disallowed_types)] // tests construct opaque OpenAPI Value docs
mod tests {
    use super::*;
    use serde_json::{Value, json};

    // ORCA_SPECS_DIR / ORCA_DB_PATH are process-global; serialize every test
    // that mutates them so parallel `cargo test` runs stay deterministic.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Point the specs dir + a fresh DB at temp dirs, then run `f`.
    /// `f` receives the specs dir path. Async bodies are driven on a
    /// current-thread runtime so the env guard is never held across `.await`.
    fn with_isolated_env<F, Fut, T>(f: F) -> T
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let specs = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        // SAFETY: set_var is single-threaded here — ENV_LOCK serializes callers.
        unsafe {
            std::env::set_var("ORCA_SPECS_DIR", specs.path());
        }
        let db_path = db.path().join("orca.db");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = rt.block_on(crate::with_db_path(db_path, f(specs.path().to_path_buf())));
        unsafe {
            std::env::remove_var("ORCA_SPECS_DIR");
        }
        out
    }

    fn sample_entry() -> SpecEntry {
        SpecEntry {
            repo: "myrepo".into(),
            project: "myproject".into(),
            description: Some("A test repo".into()),
            source: "manual".into(),
            base_url: Some("https://api.example.com".into()),
            captured_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    // ── specs_dir ──────────────────────────────────────────────────────────

    #[test]
    fn specs_dir_honors_env_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("ORCA_SPECS_DIR", "/custom/specs/path");
        }
        assert_eq!(specs_dir(), PathBuf::from("/custom/specs/path"));
        unsafe {
            std::env::remove_var("ORCA_SPECS_DIR");
        }
    }

    #[test]
    fn specs_dir_falls_back_to_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("ORCA_SPECS_DIR");
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(specs_dir(), PathBuf::from("/home/tester/.orca/specs"));
    }

    // ── serde row shapes ───────────────────────────────────────────────────

    #[test]
    fn spec_meta_row_serializes_camel_case_and_skips_none() {
        let row = SpecMetaRow {
            repo: "r".into(),
            project: "p".into(),
            source: "manual".into(),
            namespace: "orca".into(),
            source_mcp: None,
            base_url: Some("https://x".into()),
            captured_at: None,
            path_count: Some(7),
            has_graphql: true,
            files: SpecFilesPresence {
                full: true,
                public: false,
            },
        };
        let v: Value = serde_json::to_value(&row).unwrap();
        assert_eq!(v["baseUrl"], "https://x");
        assert_eq!(v["pathCount"], 7);
        assert_eq!(v["hasGraphql"], true);
        assert_eq!(v["files"]["full"], true);
        assert!(v.get("sourceMcp").is_none());
        assert!(v.get("capturedAt").is_none());

        // round-trips
        let back: SpecMetaRow = serde_json::from_value(v).unwrap();
        assert_eq!(back.repo, "r");
        assert_eq!(back.path_count, Some(7));
    }

    #[test]
    fn db_spec_row_and_register_result_skip_none() {
        let row = DbSpecRow {
            name: "n".into(),
            url: None,
            source_mcp: None,
            path_count: None,
            cached_at: None,
            enabled: true,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["name"], "n");
        assert_eq!(v["enabled"], true);
        assert!(v.get("url").is_none());
        assert!(v.get("sourceMcp").is_none());

        let res = RegisterSpecResult {
            name: "n".into(),
            url: Some("u".into()),
            source_mcp: Some("mcp1".into()),
            path_count: Some(3),
            cached_at: Some("t".into()),
            enabled: false,
        };
        let rv = serde_json::to_value(&res).unwrap();
        assert_eq!(rv["sourceMcp"], "mcp1");
        assert_eq!(rv["pathCount"], 3);
        assert_eq!(rv["enabled"], false);
    }

    #[test]
    fn sync_mcp_specs_result_serializes() {
        let r = SyncMcpSpecsResult {
            server: "s".into(),
            synced: 2,
            errors: vec!["boom".into()],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["server"], "s");
        assert_eq!(v["synced"], 2);
        assert_eq!(v["errors"][0], "boom");
    }

    #[test]
    fn spec_entry_serializes_renamed_and_skips() {
        let mut e = sample_entry();
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["baseUrl"], "https://api.example.com");
        assert_eq!(v["capturedAt"], "2026-01-01T00:00:00Z");
        assert_eq!(v["repo"], "myrepo");

        e.base_url = None;
        e.captured_at = None;
        e.description = None;
        let v2 = serde_json::to_value(&e).unwrap();
        assert!(v2.get("baseUrl").is_none());
        assert!(v2.get("capturedAt").is_none());
        assert!(v2.get("description").is_none());
    }

    // ── scaffold builders ──────────────────────────────────────────────────

    #[test]
    fn full_spec_has_expected_shape() {
        let spec = scaffold::full_spec(&sample_entry());
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(spec["info"]["title"], "myrepo");
        assert_eq!(spec["info"]["version"], "0.0.0");
        assert_eq!(spec["info"]["description"], "A test repo");
        assert_eq!(spec["x-orca"]["repo"], "myrepo");
        assert_eq!(spec["x-orca"]["project"], "myproject");
        assert_eq!(spec["x-orca"]["source"], "manual");
        assert_eq!(spec["x-orca"]["capturedAt"], "2026-01-01T00:00:00Z");
        // base_url present -> one server entry
        assert_eq!(spec["servers"][0]["url"], "https://api.example.com");
        assert_eq!(spec["servers"][0]["description"], "Production");
        // full spec carries both public + internal tags
        let tags = spec["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0]["name"], "public");
        assert_eq!(tags[1]["name"], "internal");
        assert!(spec["paths"].as_object().unwrap().is_empty());
    }

    #[test]
    fn full_spec_without_base_url_or_description_uses_defaults() {
        let entry = SpecEntry {
            repo: "bare".into(),
            project: "bare".into(),
            description: None,
            source: "manual".into(),
            base_url: None,
            captured_at: None,
        };
        let spec = scaffold::full_spec(&entry);
        assert_eq!(spec["info"]["description"], "");
        assert!(spec["servers"].as_array().unwrap().is_empty());
        // captured_at defaults to a generated timestamp (non-empty)
        assert!(!spec["x-orca"]["capturedAt"].as_str().unwrap().is_empty());
        assert_eq!(spec["x-orca"]["baseUrl"], Value::Null);
    }

    #[test]
    fn public_spec_has_suffix_and_single_tag() {
        let spec = scaffold::public_spec(&sample_entry());
        assert_eq!(spec["info"]["title"], "myrepo (Public API)");
        let tags = spec["tags"].as_array().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0]["name"], "public");
    }

    // ── filter_orca_public ─────────────────────────────────────────────────

    #[test]
    fn filter_orca_public_keeps_public_removes_internal_and_prunes() {
        let spec = json!({
            "openapi": "3.1.0",
            "paths": {
                "/docs": { "get": { "tags": ["docs"], "summary": "public docs" } },
                "/library": { "get": { "tags": ["library"] } },
                "/admin": { "get": { "tags": ["internal"] } },
                "/mixed": {
                    "get": { "tags": ["docs"] },
                    "post": { "tags": ["internal"] }
                }
            },
            "tags": [
                { "name": "docs" },
                { "name": "library" },
                { "name": "internal" }
            ]
        });
        let filtered = filter_orca_public(spec);
        let paths = filtered["paths"].as_object().unwrap();
        // internal-only path removed entirely
        assert!(!paths.contains_key("/admin"));
        // public paths retained
        assert!(paths.contains_key("/docs"));
        assert!(paths.contains_key("/library"));
        // mixed path kept but internal op stripped
        assert!(paths.contains_key("/mixed"));
        assert!(paths["/mixed"].get("post").is_none());
        assert!(paths["/mixed"].get("get").is_some());
        // unused "internal" tag pruned; public tags kept
        let tag_names: Vec<&str> = filtered["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(tag_names.contains(&"docs"));
        assert!(tag_names.contains(&"library"));
        assert!(!tag_names.contains(&"internal"));
    }

    #[test]
    fn filter_orca_public_handles_untagged_and_missing_paths() {
        // op with no tags is dropped
        let spec = json!({
            "paths": { "/x": { "get": { "summary": "no tags" } } },
            "tags": []
        });
        let filtered = filter_orca_public(spec);
        assert!(filtered["paths"].as_object().unwrap().is_empty());

        // spec without a paths object survives untouched
        let no_paths = json!({ "openapi": "3.1.0" });
        let filtered2 = filter_orca_public(no_paths);
        assert_eq!(filtered2["openapi"], "3.1.0");
    }

    // ── SpecRegistry ───────────────────────────────────────────────────────

    #[test]
    fn registry_load_missing_is_empty() {
        with_isolated_env(|_dir| async {
            let reg = SpecRegistry::load().unwrap();
            assert!(reg.entries.is_empty());
        });
    }

    #[test]
    fn registry_add_scaffolds_files_and_persists() {
        with_isolated_env(|dir| async move {
            let mut reg = SpecRegistry::load().unwrap();
            let full_path = reg.add(sample_entry()).unwrap();

            // full + public scaffolds written
            assert!(full_path.exists());
            assert_eq!(full_path, dir.join("myrepo.json"));
            assert!(dir.join("myrepo.public.json").exists());
            assert!(dir.join("registry.json").exists());
            assert_eq!(reg.entries.len(), 1);

            // reload sees the persisted entry
            let reloaded = SpecRegistry::load().unwrap();
            assert_eq!(reloaded.entries.len(), 1);
            assert_eq!(reloaded.entries[0].repo, "myrepo");

            // adding same repo updates in place (no duplicate)
            let mut updated = sample_entry();
            updated.project = "renamed".into();
            reg.add(updated).unwrap();
            assert_eq!(reg.entries.len(), 1);
            assert_eq!(reg.entries[0].project, "renamed");

            // adding a different repo appends
            let mut other = sample_entry();
            other.repo = "other".into();
            reg.add(other).unwrap();
            assert_eq!(reg.entries.len(), 2);
        });
    }

    #[test]
    fn registry_save_and_load_roundtrip() {
        with_isolated_env(|_dir| async {
            let reg = SpecRegistry {
                entries: vec![sample_entry()],
            };
            reg.save().unwrap();
            let loaded = SpecRegistry::load().unwrap();
            assert_eq!(loaded.entries.len(), 1);
            assert_eq!(
                loaded.entries[0].base_url.as_deref(),
                Some("https://api.example.com")
            );
        });
    }

    // ── ops: db-backed + validation ────────────────────────────────────────

    #[test]
    fn register_spec_requires_name_and_url() {
        with_isolated_env(|_dir| async {
            assert!(register_spec("", "http://x").await.is_err());
            assert!(register_spec("n", "").await.is_err());
        });
    }

    #[test]
    fn refresh_spec_validates_name_and_existence() {
        with_isolated_env(|_dir| async {
            // invalid characters rejected before any IO
            assert!(refresh_spec("bad name!").await.is_err());
            // valid name but no such spec
            let err = refresh_spec("ghost").await.err().unwrap().to_string();
            assert!(err.contains("no spec named"));
        });
    }

    #[test]
    fn refresh_spec_errors_when_spec_has_no_url() {
        with_isolated_env(|_dir| async {
            let conn = crate::open_default().unwrap();
            let row = openapi_specs::OpenApiSpecRow {
                name: "nourl".into(),
                url: None,
                source_mcp: None,
                spec_json: Some(r#"{"paths":{}}"#.into()),
                cached_at: None,
                enabled: true,
            };
            openapi_specs::upsert(&conn, &row).unwrap();
            let err = refresh_spec("nourl").await.err().unwrap().to_string();
            assert!(err.contains("no URL"));
        });
    }

    #[test]
    fn unregister_spec_validates_and_removes() {
        with_isolated_env(|_dir| async {
            assert!(unregister_spec("bad/name").await.is_err());
            // removing a missing spec returns false (no error)
            assert!(!unregister_spec("missing").await.unwrap());

            let conn = crate::open_default().unwrap();
            let row = openapi_specs::OpenApiSpecRow {
                name: "removable".into(),
                url: Some("http://x".into()),
                source_mcp: None,
                spec_json: None,
                cached_at: None,
                enabled: true,
            };
            openapi_specs::upsert(&conn, &row).unwrap();
            assert!(unregister_spec("removable").await.unwrap());
            assert!(!unregister_spec("removable").await.unwrap());
        });
    }

    #[test]
    fn list_db_specs_computes_path_count() {
        with_isolated_env(|_dir| async {
            let conn = crate::open_default().unwrap();
            let row = openapi_specs::OpenApiSpecRow {
                name: "counted".into(),
                url: Some("http://x".into()),
                source_mcp: None,
                spec_json: Some(r#"{"paths":{"/a":{},"/b":{}}}"#.into()),
                cached_at: Some("t".into()),
                enabled: true,
            };
            openapi_specs::upsert(&conn, &row).unwrap();

            let rows = list_db_specs().await.unwrap();
            let found = rows.iter().find(|r| r.name == "counted").unwrap();
            assert_eq!(found.path_count, Some(2));
            assert_eq!(found.url.as_deref(), Some("http://x"));
            assert!(found.enabled);
        });
    }

    #[test]
    fn list_specs_scans_disk_and_merges_db() {
        with_isolated_env(|dir| async move {
            // disk specs: a full+public pair, and a graphql-only spec
            std::fs::write(dir.join("diskapi.json"), r#"{"paths":{}}"#).unwrap();
            std::fs::write(dir.join("diskapi.public.json"), "{}").unwrap();
            std::fs::write(dir.join("gqlapi.graphql"), "type Query { x: Int }").unwrap();
            // registry.json provides project/baseUrl metadata for diskapi
            std::fs::write(
                dir.join("registry.json"),
                r#"[{"repo":"diskapi","project":"Disk Project","baseUrl":"https://d","source":"manual"}]"#,
            )
            .unwrap();

            // a db-only spec (not present on disk) should be merged in
            let conn = crate::open_default().unwrap();
            let row = openapi_specs::OpenApiSpecRow {
                name: "dbonly".into(),
                url: Some("http://db".into()),
                source_mcp: None,
                spec_json: Some(r#"{"paths":{"/z":{}}}"#.into()),
                cached_at: Some("t".into()),
                enabled: true,
            };
            openapi_specs::upsert(&conn, &row).unwrap();

            let rows = list_specs().await.unwrap();

            let disk = rows.iter().find(|r| r.repo == "diskapi").unwrap();
            assert_eq!(disk.project, "Disk Project");
            assert_eq!(disk.base_url.as_deref(), Some("https://d"));
            assert_eq!(disk.source, "manual");
            assert!(disk.files.full);
            assert!(disk.files.public);
            assert!(!disk.has_graphql);
            assert_eq!(disk.namespace, "orca");

            let gql = rows.iter().find(|r| r.repo == "gqlapi").unwrap();
            assert!(gql.has_graphql);
            assert!(!gql.files.full);
            // project falls back to repo when no registry metadata
            assert_eq!(gql.project, "gqlapi");
            assert_eq!(gql.source, "manual");

            let dbonly = rows.iter().find(|r| r.repo == "dbonly").unwrap();
            assert_eq!(dbonly.source, "url");
            assert_eq!(dbonly.path_count, Some(1));
            assert_eq!(dbonly.base_url.as_deref(), Some("http://db"));
            assert!(dbonly.files.full);
        });
    }
}
