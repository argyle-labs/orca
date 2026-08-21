//! Drift anchor for `docs/plugin-authoring/`.
//!
//! Every code snippet in the plugin-authoring docs is pinned here against real
//! `plugin-toolkit` source. If a documented symbol is renamed, removed, or has
//! its signature changed, THIS FILE STOPS COMPILING and orca CI goes red — so a
//! doc can never silently drift from the toolkit it teaches. `cargo nextest`
//! (CI) compiles and runs it; doctests are not run by nextest, which is why this
//! is a real integration test rather than a `rust,ignore` block.
//!
//! Two guards:
//!   1. `documented_symbols_exist` — signature/type-pins for every cited symbol.
//!   2. `doc_paths_resolve` — every `projects/…` path (and `:line`) the docs
//!      cite still exists on disk.
#![allow(clippy::disallowed_types)]
#![allow(unused_imports)]

use plugin_toolkit::prelude::*;

// ── Pins the CORRECTED tool signature the docs teach: ────────────────────────
//    (args, &ToolCtx) -> anyhow::Result<T>. The prior docs showed a fictional
//    `OrcaError` and a missing args param; if #[orca_tool], ToolCtx, or the
//    anyhow::Result contract changes, this fails to compile.
#[orca_struct(args)]
pub struct ServerInfoArgs {}

#[orca_struct]
pub struct ServerInfoOut {
    pub version: String,
}

#[orca_tool(domain = "doc_anchor", verb = "server_info")]
pub async fn server_info(_args: ServerInfoArgs, _ctx: &ToolCtx) -> anyhow::Result<ServerInfoOut> {
    Ok(ServerInfoOut {
        version: "0".into(),
    })
}

// ── Pins the CRUD attribute + its documented `plugin =`/`table =` form. It is
//    a #[proc_macro_attribute] on a struct — NOT a function-like macro. ───────
#[endpoint_resource(plugin = "doc_anchor", table = "doc_anchor_endpoints")]
struct DocAnchorEndpoint {
    base_url: String,
    token: String,
    enabled: bool,
}

// ── Pins the serve_*_plugin! family (each is #[macro_export] at the crate
//    root). A rename breaks this import. ──────────────────────────────────────
use plugin_toolkit::{
    serve_backup_kind_plugin, serve_backup_target_plugin, serve_service_plugin,
    serve_storage_plugin, serve_tool_plugin,
};

#[test]
fn documented_symbols_exist() {
    // backend_def surface — signature-pinned via fn-pointer coercion.
    let _: fn(&str, &str) -> plugin_toolkit::abi::BackendDef =
        plugin_toolkit::backend_def::secrets_backend_def;
    let _: fn(&str, &str) -> String = plugin_toolkit::backend_def::secrets_backends_json;
    let _: fn(&dyn plugin_toolkit::contract::unit::UnitProvider, &str) -> String =
        plugin_toolkit::backend_def::unit_backends_json;

    // secrets-backend resolve op the onepassword example matches on.
    assert_eq!(
        plugin_toolkit::contract::secrets_backend::RESOLVE_OP,
        "resolve"
    );

    // HTTP client seam used in the tool + capabilities pages.
    let _client = plugin_toolkit::client::Client::new();
}

/// Every in-repo `projects/…` path (optionally `:line`) the plugin-authoring
/// docs cite must resolve. Cross-repo `argyle-labs/*` references are
/// illustrative and intentionally not checked.
#[test]
fn doc_paths_resolve() {
    use std::path::PathBuf;

    // CARGO_MANIFEST_DIR = <root>/projects/plugin-toolkit
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root is two levels above the crate manifest")
        .to_path_buf();

    let docs_dir = repo_root.join("docs/plugin-authoring");
    let mut checked = 0usize;

    for entry in std::fs::read_dir(&docs_dir).expect("docs/plugin-authoring must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap();
        for raw in body.split(|c: char| c.is_whitespace() || "()[]`\"'<>".contains(c)) {
            let tok = raw.trim_start_matches("../").trim_start_matches("../");
            if !tok.starts_with("projects/") {
                continue;
            }
            // Split a trailing `:<line>` if present.
            let (rel, line) = match tok.rsplit_once(':') {
                Some((p, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
                    (p, Some(n.parse::<usize>().unwrap()))
                }
                _ => (tok, None),
            };
            if !(rel.ends_with(".rs") || rel.ends_with(".md")) {
                continue;
            }
            let full = repo_root.join(rel);
            assert!(
                full.exists(),
                "{}: cites missing path `{rel}`",
                path.display()
            );
            if let Some(n) = line {
                let count = std::fs::read_to_string(&full).unwrap().lines().count();
                assert!(
                    count >= n,
                    "{}: cites `{rel}:{n}` but that file has only {count} lines",
                    path.display()
                );
            }
            checked += 1;
        }
    }

    assert!(
        checked > 0,
        "expected to path-check at least one projects/ citation"
    );
}
