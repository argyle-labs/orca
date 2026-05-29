//! Generate a typed Sonarr client from the vendored OpenAPI spec.
//! Output lands in `$OUT_DIR/sonarr_codegen.rs` and is `include!`d by lib.rs.
//!
//! All "make this upstream spec digestible by progenitor" work lives in
//! `integrations_openapi::normalize` — never patch the spec here.

use std::{env, fs, path::PathBuf};

fn main() {
    let spec_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sonarr.openapi.json");
    println!("cargo:rerun-if-changed={}", spec_path.display());

    let raw = fs::read_to_string(&spec_path).expect("read sonarr.openapi.json");
    let mut spec: openapiv3::OpenAPI = serde_json::from_str(&raw).expect("parse sonarr openapi");
    integrations_openapi::normalize::for_progenitor(&mut spec);

    let mut generator = progenitor::Generator::default();
    let tokens = generator
        .generate_tokens(&spec)
        .expect("progenitor codegen");

    let ast = syn::parse2(tokens).expect("parse generated tokens");
    let content = prettyplease::unparse(&ast);

    let mut out = PathBuf::from(env::var("OUT_DIR").unwrap());
    out.push("sonarr_codegen.rs");
    fs::write(out, content).expect("write generated client");
}
