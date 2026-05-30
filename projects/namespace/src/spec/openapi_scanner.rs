//! OpenAPI scanner — scaffold synthesis for tracked specs and the
//! orca-public-spec filter. Builds dynamic OpenAPI documents; `serde_json::Value`
//! is the documented escape hatch for arbitrary OpenAPI shape.
#![allow(clippy::disallowed_types)] // OpenAPI document builder — dynamic JSON construction

use super::registry::SpecEntry;
use serde_json::{Value, json};

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

/// Domain tags in orca's own spec that are publicly accessible.
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

/// Filter orca's own spec to only operations in publicly accessible domain groups.
/// Uses domain tags (docs, library) since utoipa 4.x doesn't support multi-tag paths.
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
