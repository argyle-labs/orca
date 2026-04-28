use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

pub fn openapi_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("brain/openapi")
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
        let path = openapi_dir().join("registry.json");
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self { entries })
    }

    pub fn save(&self) -> Result<()> {
        let dir = openapi_dir();
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

        let dir = openapi_dir();
        let full_path = dir.join(format!("{}.json", entry.repo));
        let public_path = dir.join(format!("{}.public.json", entry.repo));

        if !full_path.exists() {
            let scaffold = scaffold_full_spec(&entry);
            std::fs::write(&full_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        if !public_path.exists() {
            let scaffold = scaffold_public_spec(&entry);
            std::fs::write(&public_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        Ok(full_path)
    }
}

fn base_spec_info(entry: &SpecEntry, title_suffix: &str) -> Value {
    let now = chrono::Utc::now().to_rfc3339();
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
        "x-brain": {
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
pub fn scaffold_full_spec(entry: &SpecEntry) -> Value {
    let mut spec = base_spec_info(entry, "");
    spec["tags"] = json!([
        { "name": "public",   "description": "Publicly accessible endpoints" },
        { "name": "internal", "description": "Internal endpoints — not for external consumers" }
    ]);
    spec
}

/// Standalone public spec scaffold — complete, self-contained, public endpoints only.
/// This is NOT a filtered derivative — it is independently maintained.
pub fn scaffold_public_spec(entry: &SpecEntry) -> Value {
    let mut spec = base_spec_info(entry, " (Public API)");
    spec["tags"] = json!([
        { "name": "public", "description": "Publicly accessible endpoints" }
    ]);
    spec
}

const METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Domain tags in brain's own spec that are publicly accessible.
/// utoipa 4.x only supports one tag per path, so we classify by domain name.
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

/// Filter brain's own spec to only operations in publicly accessible domain groups.
/// Uses domain tags (docs, library) since utoipa 4.x doesn't support multi-tag paths.
pub fn filter_brain_public(spec: Value) -> Value {
    let mut filtered = filter_ops(spec, |op| {
        op["tags"]
            .as_array()
            .map(|tags| {
                tags.iter()
                    .any(|t| BRAIN_PUBLIC_DOMAINS.contains(&t.as_str().unwrap_or("")))
            })
            .unwrap_or(false)
    });

    // Collect tags actually referenced in the surviving paths.
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
