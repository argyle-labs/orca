//! Thin-profile tool-surface helpers a subprocess plugin needs, with no reactor
//! or transport dependency.
//!
//! [`serve`](crate::serve) (the out-of-process socket loop) filters its
//! `#[orca_tool]` manifest and runs tools against a [`minimal_ctx`]. Those two
//! functions only walk the dispatch inventory and build a `ToolCtx`, so they
//! are gated on `tools` alone and link neither tokio nor any transport.
#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use serde_json as sj;

use crate::abi::ToolDef;
use crate::contract::ToolCtx;
use crate::contract::config::{Config, Model, Ports};

/// Minimal off-orca [`ToolCtx`] a plugin runs its tools against when it holds no
/// real orca services: no model creds, temp-dir paths, default ports. A tool
/// that needs live orca services reaches them over a capability round-trip
/// (subprocess) or the ABI (cdylib), not through this stub ctx.
pub fn minimal_ctx() -> ToolCtx {
    let config = Config {
        anthropic_api_key: None,
        lmstudio_url: String::new(),
        ollama_url: String::new(),
        default_model: Model::LMStudio {
            id: String::new(),
            url: String::new(),
        },
        app_dir: std::env::temp_dir(),
        memory_root: std::env::temp_dir(),
        db_path: std::env::temp_dir().join("orca-plugin.db"),
        ports: Ports::default(),
    };
    ToolCtx::new(Arc::new(config))
}

/// Filter the statically-linked `#[orca_tool]` inventory down to this plugin's
/// `prefix` (trailing dot included) and return the manifest JSON. The
/// single-prefix case; see [`manifest_for_prefixes`] for a plugin (like `arr`)
/// that hosts several app namespaces.
pub fn manifest_for(prefix: &str) -> String {
    manifest_for_prefixes(&[prefix])
}

/// Filter the linked `#[orca_tool]` inventory down to ANY of this plugin's
/// `prefixes` (each trailing-dot included) and return the manifest JSON. A
/// multi-app plugin — `arr` hosting `sonarr.`/`radarr.`/`prowlarr.`/`lidarr.` —
/// exposes every app it owns through one plugin by listing all their prefixes;
/// the plugin also links the toolkit's domain crates, whose inventory entries
/// the raw walk returns, so the filter keeps only the plugin's own namespaces.
pub fn manifest_for_prefixes(prefixes: &[&str]) -> String {
    let all: Vec<ToolDef> =
        sj::from_str(&crate::dispatch::tool_manifest_json()).unwrap_or_default();
    let mine: Vec<ToolDef> = all
        .into_iter()
        .filter(|d| prefixes.iter().any(|p| d.name.starts_with(p)))
        .collect();
    sj::to_string(&mine).unwrap_or_else(|_| "[]".to_string())
}
