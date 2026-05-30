//! Spec registry shared primitives — `specs_dir()`, `SpecEntry`,
//! `SpecRegistry`. Used by both the OpenAPI and GraphQL scanners.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
            let scaffold = super::openapi_scanner::scaffold_full_spec(&entry);
            std::fs::write(&full_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        if !public_path.exists() {
            let scaffold = super::openapi_scanner::scaffold_public_spec(&entry);
            std::fs::write(&public_path, serde_json::to_string_pretty(&scaffold)?)?;
        }
        Ok(full_path)
    }
}
