//! Build-time GraphQL codegen for the typed Unraid client.
//!
//! For each `.graphql` query file under `queries/`, invokes
//! `graphql_client_codegen::generate_module_token_stream` against the
//! committed introspection JSON in
//! `../unraid/schemas/<version>.introspection.json` and concatenates the
//! emitted token streams into a single per-version file in `OUT_DIR`.
//! `src/lib.rs` then `include!`s that file.
//!
//! Slice A only wires `v7_3_1`. Slice B will extend this to walk every
//! schema in the schemas directory and emit one module per version.

use graphql_client_codegen::{
    CodegenMode, GraphQLClientCodegenOptions, generate_module_token_stream,
};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));

    let schema = manifest_dir
        .join("..")
        .join("unraid")
        .join("schemas")
        .join("7.3.1.introspection.json");
    println!("cargo:rerun-if-changed={}", schema.display());

    let queries_dir = manifest_dir.join("queries");
    let query_files = [
        "installed_plugins.graphql",
        "add_plugin.graphql",
        "remove_plugin.graphql",
        "array.graphql",
        "shares.graphql",
        "parity_history.graphql",
    ];

    let mut combined = proc_macro2::TokenStream::new();
    for q in query_files {
        let qpath = queries_dir.join(q);
        println!("cargo:rerun-if-changed={}", qpath.display());
        combined.extend(emit(&qpath, &schema));
    }

    let dest = out_dir.join("v7_3_1.rs");
    std::fs::write(&dest, combined.to_string()).expect("write v7_3_1.rs");
}

fn emit(query_path: &Path, schema_path: &Path) -> proc_macro2::TokenStream {
    let mut opts = GraphQLClientCodegenOptions::new(CodegenMode::Cli);
    opts.set_module_visibility(syn::parse_quote!(pub));
    opts.set_response_derives("Debug,Clone".to_string());
    opts.set_variables_derives("Debug,Clone".to_string());
    generate_module_token_stream(query_path.to_path_buf(), schema_path, opts)
        .unwrap_or_else(|e| panic!("codegen {}: {e}", query_path.display()))
}
