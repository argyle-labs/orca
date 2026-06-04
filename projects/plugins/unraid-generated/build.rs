//! Build-time GraphQL codegen for the typed Unraid client.
//!
//! Walks every `<version>.introspection.json` in
//! `../unraid/schemas/`, runs `graphql_client_codegen` over the
//! `.graphql` query files in `queries/` against each schema, and emits
//! one module per version into `OUT_DIR`. `lib.rs` includes the
//! aggregated output. The set of supported versions is therefore
//! whatever is committed on disk — adding 7.3.0 means dropping
//! `schemas/7.3.0.introspection.json` in and rebuilding.

use graphql_client_codegen::{
    CodegenMode, GraphQLClientCodegenOptions, generate_module_token_stream,
};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let schemas_dir = manifest_dir.join("..").join("unraid").join("schemas");
    let queries_dir = manifest_dir.join("queries");

    println!("cargo:rerun-if-changed={}", schemas_dir.display());
    println!("cargo:rerun-if-changed={}", queries_dir.display());

    let query_files = collect_files(&queries_dir, "graphql");
    for q in &query_files {
        println!("cargo:rerun-if-changed={}", q.display());
    }

    let mut versions: Vec<(String, PathBuf)> = collect_files(&schemas_dir, "json")
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            // schemas/<version>.introspection.json
            let stripped = name.strip_suffix(".introspection.json")?;
            Some((stripped.to_string(), p))
        })
        .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !versions.is_empty(),
        "no schemas in {}",
        schemas_dir.display()
    );

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
        combined.push_str(&tokens.to_string());
        combined.push_str("}\npub use generated::*;\n");
        combined.push_str("}\n");
        supported.push((version.clone(), module));
    }

    combined.push_str("pub const SUPPORTED_VERSIONS: &[(&str, &str)] = &[");
    for (v, m) in &supported {
        combined.push_str(&format!("(\"{v}\", \"{m}\"),"));
    }
    combined.push_str("];\n");

    // Embed each committed introspection JSON as a `&'static str` so the
    // unraid crate can compare live vs committed bytes without going to
    // disk. `include_str!` paths must be literals, so we expand one per
    // schema file at build time.
    combined.push_str("pub const SCHEMAS: &[(&str, &str)] = &[");
    for (version, schema_path) in &versions {
        let escaped = schema_path.display().to_string().replace('\\', "\\\\");
        combined.push_str(&format!("(\"{version}\", include_str!(\"{escaped}\")),"));
    }
    combined.push_str("];\n");

    let dest = out_dir.join("modules.rs");
    std::fs::write(&dest, combined).expect("write modules.rs");
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
    opts.set_module_visibility(syn::parse_quote!(pub));
    opts.set_response_derives("Debug,Clone".to_string());
    opts.set_variables_derives("Debug,Clone".to_string());
    generate_module_token_stream(query_path.to_path_buf(), schema_path, opts)
        .unwrap_or_else(|e| panic!("codegen {}: {e}", query_path.display()))
}
