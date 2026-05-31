//! Plugin manifest (`orca-plugin.toml`) — canonical source of transport, surface,
//! and per-plugin metadata. The host reads this on disk (path stored in
//! `plugins.manifest_path`) every time it needs to dial a plugin instead of
//! caching transport in DB columns.
//!
//! Lives in `db` so both `plugins/runtime/install` (registration time) and the
//! dial-time consumers (`mcp::client`, `db::plugin_creds::sync`) can share one
//! parser without crossing crate-graph constraints. The SDK ships its own
//! strict v0 [`sdk::manifest::Manifest`]; reconciling the two is the next wave
//! after this column-removal pass.
#![allow(clippy::disallowed_types)] // nav_links is plugin-defined free-form JSON

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

use crate::PluginSearchTool;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub plugin: PluginSection,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginSection {
    pub id: String,
    pub version: String,
    pub tier: String,
    #[serde(default)]
    pub context_injection: Option<String>,
    #[serde(default)]
    pub mcp: Option<McpSection>,
    #[serde(default)]
    pub commands: HashMap<String, String>,
    #[serde(default)]
    pub nav_links: Vec<serde_json::Value>,
    #[serde(default)]
    pub search_tools: Vec<PluginSearchTool>,
    #[serde(default)]
    pub specs: Option<SpecsSection>,
    #[serde(default, rename = "uses")]
    pub uses: Vec<UsesSection>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpSection {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Env var name whose value is the Bearer token for HTTP/SSE transport.
    pub token_env: Option<String>,
    /// Shorthand for a single-entry `urls` list.
    pub url: Option<String>,
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SpecsSection {
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsesSection {
    pub path: String,
    pub id: Option<String>,
}

impl McpSection {
    /// Effective URL list — `urls` wins, falling back to the single-`url` shorthand.
    pub fn urls(&self) -> Vec<String> {
        if !self.urls.is_empty() {
            self.urls.clone()
        } else if let Some(u) = &self.url {
            vec![u.clone()]
        } else {
            vec![]
        }
    }

    pub fn command_nonempty(&self) -> Option<&str> {
        if self.command.is_empty() {
            None
        } else {
            Some(&self.command)
        }
    }
}

impl Manifest {
    /// Canonical HTTP base for this plugin — first `mcp.urls` entry, else an
    /// `http(s)://` `mcp.command`, else an `http(s)://` arg. Trailing slashes
    /// stripped so callers can append paths cleanly. `None` for stdio plugins.
    pub fn resolve_url(&self) -> Option<String> {
        let mcp = self.plugin.mcp.as_ref()?;
        if let Some(u) = mcp.urls().into_iter().next() {
            return Some(u.trim_end_matches('/').to_string());
        }
        if (mcp.command.starts_with("http://") || mcp.command.starts_with("https://"))
            && !mcp.command.is_empty()
        {
            return Some(mcp.command.trim_end_matches('/').to_string());
        }
        for arg in &mcp.args {
            if arg.starts_with("http://") || arg.starts_with("https://") {
                return Some(arg.trim_end_matches('/').to_string());
            }
        }
        None
    }
}

/// Parse a manifest from a file on disk. Tilde-expands the path and canonicalizes.
pub fn parse_path(path: &str) -> Result<(Manifest, String)> {
    let resolved = utils::path::expand_tilde(path);
    let abs = std::fs::canonicalize(&resolved)
        .with_context(|| format!("manifest not found: {resolved}"))?;
    let text = std::fs::read_to_string(&abs)
        .with_context(|| format!("failed to read {}", abs.display()))?;
    let manifest: Manifest = toml::from_str(&text)
        .with_context(|| format!("invalid orca-plugin.toml at {}", abs.display()))?;
    Ok((manifest, abs.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("orca-plugin.toml");
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn parse_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            r#"
[plugin]
id = "x"
version = "1.0.0"
tier = "personal"
"#,
        );
        let (m, abs) = parse_path(&path).unwrap();
        assert_eq!(m.plugin.id, "x");
        assert!(abs.ends_with("orca-plugin.toml"));
        assert!(m.resolve_url().is_none());
    }

    #[test]
    fn resolve_url_prefers_urls_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            r#"
[plugin]
id = "x"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = "node"
args = ["server.js"]
url = "http://public"
urls = ["http://lan/", "http://ts"]
"#,
        );
        let (m, _) = parse_path(&path).unwrap();
        assert_eq!(m.resolve_url().as_deref(), Some("http://lan"));
    }

    #[test]
    fn resolve_url_from_http_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            r#"
[plugin]
id = "x"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = "https://api.example.com/"
"#,
        );
        let (m, _) = parse_path(&path).unwrap();
        assert_eq!(m.resolve_url().as_deref(), Some("https://api.example.com"));
    }

    #[test]
    fn resolve_url_from_http_arg() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest(
            dir.path(),
            r#"
[plugin]
id = "x"
version = "1.0.0"
tier = "personal"

[plugin.mcp]
command = "node"
args = ["server.js", "http://localhost:9000"]
"#,
        );
        let (m, _) = parse_path(&path).unwrap();
        assert_eq!(m.resolve_url().as_deref(), Some("http://localhost:9000"));
    }

    #[test]
    fn missing_file_errors() {
        match parse_path("/tmp/__nope__.toml") {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("manifest not found")),
        }
    }
}
