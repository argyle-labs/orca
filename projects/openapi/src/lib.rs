//! Generic OpenAPI integration. Parses any OpenAPI 3.0/3.1 spec from a file
//! path, string, or URL, and surfaces a navigable view (paths, operations,
//! schemas) that orca uses for:
//!   - the spec registry (`specs` crate) → MCP / UI exposure of arbitrary specs
//!   - per-integration typed clients (`integrations/sonarr`, etc.) whose
//!     `build.rs` runs `progenitor` against an embedded spec file.
//!
//! No typed-client codegen lives here: `progenitor` must run inside the
//! consuming crate's build script, so each integration depends on
//! `progenitor` directly as a build-dependency.
//!
//! `serde_json::Value` is intentional: OpenAPI extensions (`x-…`) and
//! example values are arbitrary JSON the upstream owns.
#![allow(clippy::disallowed_types)]

use anyhow::{Context, Result};
use oas3::Spec;
use serde::Serialize;
use std::path::Path;

pub mod lower_31;
pub mod normalize;

/// Lightweight per-operation view used by the spec registry / MCP / UI.
#[derive(Debug, Clone, Serialize)]
pub struct OperationSummary {
    pub method: String,
    pub path: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
}

/// Parse a spec from raw text. Accepts JSON or YAML.
pub fn parse_str(raw: &str) -> Result<Spec> {
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).context("openapi: invalid JSON spec")
    } else {
        utils::yaml::from_str(raw).context("openapi: invalid YAML spec")
    }
}

/// Parse a spec from disk.
pub fn parse_file(path: impl AsRef<Path>) -> Result<Spec> {
    let raw = std::fs::read_to_string(path.as_ref())
        .with_context(|| format!("openapi: read {}", path.as_ref().display()))?;
    parse_str(&raw)
}

/// Fetch a spec from a URL (blocking; intended for build scripts and
/// occasional sync operations).
pub fn fetch_blocking(url: &str) -> Result<Spec> {
    let raw = reqwest::blocking::get(url)
        .with_context(|| format!("openapi: GET {url}"))?
        .error_for_status()?
        .text()?;
    parse_str(&raw)
}

/// Fetch a spec from a URL asynchronously.
pub async fn fetch(url: &str) -> Result<Spec> {
    let raw = reqwest::get(url)
        .await
        .with_context(|| format!("openapi: GET {url}"))?
        .error_for_status()?
        .text()
        .await?;
    parse_str(&raw)
}

/// Flatten a spec into a list of `(method, path, operationId, summary, tags)`.
/// Operations are emitted in spec order; the registry/UI groups by tag.
pub fn operations(spec: &Spec) -> Vec<OperationSummary> {
    let mut out = Vec::new();
    let Some(paths) = spec.paths.as_ref() else {
        return out;
    };
    for (path, item) in paths {
        for (method, op) in [
            ("GET", &item.get),
            ("PUT", &item.put),
            ("POST", &item.post),
            ("DELETE", &item.delete),
            ("OPTIONS", &item.options),
            ("HEAD", &item.head),
            ("PATCH", &item.patch),
            ("TRACE", &item.trace),
        ] {
            if let Some(op) = op {
                out.push(OperationSummary {
                    method: method.to_string(),
                    path: path.clone(),
                    operation_id: op.operation_id.clone(),
                    summary: op.summary.clone(),
                    tags: op.tags.clone(),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINI: &str = r#"{
        "openapi": "3.0.0",
        "info": {"title": "t", "version": "1"},
        "paths": {
            "/ping": {"get": {"operationId": "ping", "summary": "ping", "responses": {"200": {"description": "ok"}}}}
        }
    }"#;

    #[test]
    fn parses_json() {
        let s = parse_str(MINI).unwrap();
        let ops = operations(&s);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].method, "GET");
        assert_eq!(ops[0].path, "/ping");
        assert_eq!(ops[0].operation_id.as_deref(), Some("ping"));
    }
}
