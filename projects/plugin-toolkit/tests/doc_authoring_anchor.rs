//! Drift anchor for `docs/plugin-authoring/`.
//!
//! Every code snippet in the plugin-authoring docs is pinned here against real
//! `plugin-toolkit` source. If a documented symbol is renamed, removed, or has
//! its signature changed, THIS FILE STOPS COMPILING and orca CI goes red — so a
//! doc can never silently drift from the toolkit it teaches. `cargo nextest`
//! (CI) compiles and runs it; doctests are not run by nextest, which is why this
//! is a real integration test rather than a `rust,ignore` block.
//!
//! `documented_symbols_exist` signature/type-pins every cited symbol. The
//! docs' `projects/…` path citations are checked by
//! `projects/inventory-tests/tests/plugin_authoring_doc_paths.rs`.
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

#[test]
fn documented_symbols_exist() {
    // backend_def surface — signature-pinned via fn-pointer coercion.
    let _: fn(&str, &str) -> plugin_toolkit::abi::BackendDef =
        plugin_toolkit::backend_def::secrets_backend_def;
    // The one generic backends() serializer that replaced the per-domain
    // `*_backends_json` wrappers.
    let _: fn(Vec<plugin_toolkit::abi::BackendDef>) -> String =
        plugin_toolkit::backend_def::backends_json;
    let _: fn(
        &dyn plugin_toolkit::contract::unit::UnitProvider,
        &str,
    ) -> plugin_toolkit::abi::BackendDef = plugin_toolkit::backend_def::unit_backend_def;

    // secrets-backend resolve op the onepassword example matches on.
    assert_eq!(
        plugin_toolkit::contract::secrets_backend::RESOLVE_OP,
        "resolve"
    );

    // HTTP client seam used in the tool + capabilities pages.
    let _client = plugin_toolkit::client::Client::new();
}
