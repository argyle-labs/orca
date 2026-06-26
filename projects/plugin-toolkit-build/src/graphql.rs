//! GraphQL codegen helper for plugin `build.rs` files.
//!
//! Walks `<schemas_dir>/<version>.introspection.json` + `<queries_dir>/*.graphql`
//! and emits one Rust module per version into `<OUT_DIR>/modules.rs`. The
//! plugin includes `modules.rs` from `src/lib.rs`.
//!
//! Emitted module layout (per schema version `v_1`):
//!
//! ```rust,ignore
//! pub mod v_1 {
//!     pub type BigInt = String;
//!     pub type PrefixedID = String;
//!     pub type DateTime = String;
//!     mod generated { use super::{BigInt, DateTime, PrefixedID};
//!         // graphql_client_codegen output for each .graphql file
//!     }
//!     pub use generated::*;
//! }
//! ```
//!
//! Plus top-level constants:
//! - `SUPPORTED_VERSIONS: &[(&str, &str)]` — `(version, module_ident)` pairs
//! - `SCHEMAS: &[(&str, &str)]` — `(version, schema_json_bytes)` pairs via
//!   `include_str!`, so the plugin can drift-check at runtime without I/O.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use graphql_client_codegen::{
    CodegenMode, GraphQLClientCodegenOptions, generate_module_token_stream,
};

/// Generate the versioned GraphQL client module tree.
///
/// `schemas_dir` must contain one or more `<version>.introspection.json`
/// files. `queries_dir` contains the `.graphql` query files run against
/// every schema version.
pub fn generate(schemas_dir: impl AsRef<Path>, queries_dir: impl AsRef<Path>) -> Result<()> {
    let schemas_dir = schemas_dir.as_ref();
    let queries_dir = queries_dir.as_ref();
    let out_dir = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .context("OUT_DIR not set — generate must be called from build.rs")?;

    println!("cargo:rerun-if-changed={}", schemas_dir.display());
    println!("cargo:rerun-if-changed={}", queries_dir.display());

    let query_files = collect_files(queries_dir, "graphql");
    for q in &query_files {
        println!("cargo:rerun-if-changed={}", q.display());
    }

    let mut versions: Vec<(String, PathBuf)> = collect_files(schemas_dir, "json")
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            let stripped = name.strip_suffix(".introspection.json")?;
            Some((stripped.to_string(), p))
        })
        .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0));
    if versions.is_empty() {
        anyhow::bail!("no schemas in {}", schemas_dir.display());
    }

    let mut combined = String::new();
    let mut supported: Vec<(String, String)> = Vec::new();
    for (version, schema_path) in &versions {
        println!("cargo:rerun-if-changed={}", schema_path.display());
        let module = format!("v{}", version.replace('.', "_"));
        let mut tokens = proc_macro2::TokenStream::new();
        for q in &query_files {
            tokens.extend(emit(q, schema_path));
        }
        combined.push_str(&format!("pub mod {module} {{\n"));
        combined.push_str("pub type BigInt = String;\n");
        combined.push_str("pub type PrefixedID = String;\n");
        combined.push_str("pub type DateTime = String;\n");
        combined
            .push_str("#[allow(non_camel_case_types, unused_imports, dead_code, clippy::all)]\n");
        combined.push_str("mod generated { use super::{BigInt, DateTime, PrefixedID};\n");
        combined.push_str(&rewrite_codegen_paths(&tokens.to_string()));
        combined.push_str("}\npub use generated::*;\n");
        combined.push_str("}\n");
        supported.push((version.clone(), module));
    }

    combined.push_str("pub const SUPPORTED_VERSIONS: &[(&str, &str)] = &[");
    for (v, m) in &supported {
        combined.push_str(&format!("(\"{v}\", \"{m}\"),"));
    }
    combined.push_str("];\n");

    combined.push_str("pub const SCHEMAS: &[(&str, &str)] = &[");
    for (version, schema_path) in &versions {
        let escaped = schema_path.display().to_string().replace('\\', "\\\\");
        combined.push_str(&format!("(\"{version}\", include_str!(\"{escaped}\")),"));
    }
    combined.push_str("];\n");

    let dest = out_dir.join("modules.rs");
    std::fs::write(&dest, combined).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

fn collect_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some(ext) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn emit(query_path: &Path, schema_path: &Path) -> proc_macro2::TokenStream {
    let mut opts = GraphQLClientCodegenOptions::new(CodegenMode::Cli);
    opts.set_module_visibility(syn1::parse_quote!(pub));
    opts.set_response_derives("Debug,Clone".to_string());
    opts.set_variables_derives("Debug,Clone".to_string());
    generate_module_token_stream(query_path.to_path_buf(), schema_path, opts)
        .unwrap_or_else(|e| panic!("codegen {}: {e}", query_path.display()))
}

/// Redirect the crate-root paths `graphql_client_codegen` emits so they resolve
/// through the toolkit re-exports — a plugin then needs no direct dep on
/// `graphql_client` or `serde`. The codegen has no `crate = ...` override, so we
/// rewrite its stringified output.
fn rewrite_codegen_paths(s: &str) -> String {
    let s = redirect_crate(s, "graphql_client");
    let s = redirect_crate(&s, "serde");
    anchor_serde_derives(&s)
}

/// The codegen emits `#[derive(Serialize, …)]` / `#[derive(Deserialize, …)]`
/// without a `#[serde(crate = …)]` attribute, so the serde derive macro emits
/// `::serde::*` impl paths and the plugin would need a direct `serde` dep.
/// Inject the crate attribute (the same mechanism `#[plugin_struct]` uses) so
/// the derive resolves serde through the toolkit. `proc_macro2` renders derive
/// lists as `# [derive (Serialize , Debug , Clone)]`.
fn anchor_serde_derives(s: &str) -> String {
    const ATTR: &str = " # [serde (crate = \"::plugin_toolkit::serde\")]";
    s.replace(
        "# [derive (Serialize , Debug , Clone)]",
        &format!("# [derive (Serialize , Debug , Clone)]{ATTR}"),
    )
    .replace(
        "# [derive (Deserialize , Debug , Clone)]",
        &format!("# [derive (Deserialize , Debug , Clone)]{ATTR}"),
    )
}

/// Rewrite every `<krate>::…` path in generated code to
/// `::plugin_toolkit::<krate>::…`.
///
/// `proc_macro2`'s `to_string()` renders `::` with surrounding spaces, so paths
/// appear as `serde ::` (bare) and `:: serde ::` (already-absolute). A sentinel
/// guards the absolute form so it is not double-prefixed into
/// `:: :: plugin_toolkit`. The trailing ` ::` in the match anchors on a path
/// segment boundary, so a sibling crate like `serde_json` (rendered
/// `serde_json ::`) never matches the `serde` rule.
fn redirect_crate(s: &str, krate: &str) -> String {
    let abs = format!(":: {krate} ::");
    let bare = format!("{krate} ::");
    let target = format!(":: plugin_toolkit :: {krate} ::");
    let sentinel = format!("\u{0}{krate}\u{0}");
    let s = s.replace(&abs, &sentinel);
    let s = s.replace(&bare, &target);
    s.replace(&sentinel, &target)
}
