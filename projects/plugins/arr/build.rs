//! Generate typed clients for every *arr spec under `specs/`. One spec →
//! one module included by `src/lib.rs`. Modules are wired up in lib.rs via
//! a parallel list — add a new spec *and* a `flavor!(name)` line to enable
//! a flavor.
//!
//! All "make this upstream spec digestible by progenitor" work lives in
//! `openapi::normalize` — never patch a spec here.

use std::{env, ffi::OsStr, fs, path::PathBuf};

fn main() {
    let specs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("specs");
    println!("cargo:rerun-if-changed={}", specs_dir.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let mut entries: Vec<_> = fs::read_dir(&specs_dir)
        .expect("read specs/")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension() == Some(OsStr::new("json")))
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let spec_path = entry.path();
        println!("cargo:rerun-if-changed={}", spec_path.display());

        let flavor = spec_path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".openapi.json"))
            .expect("spec filename must be <flavor>.openapi.json");

        let raw = fs::read_to_string(&spec_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", spec_path.display()));
        let mut spec: openapiv3::OpenAPI = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("parse {}: {e}", spec_path.display()));

        let report = openapi::normalize::for_progenitor(&mut spec);
        report.emit_cargo_warnings(&format!("arr::{flavor}"));

        let tokens = progenitor::Generator::default()
            .generate_tokens(&spec)
            .unwrap_or_else(|e| panic!("progenitor codegen for {flavor}: {e}"));
        let ast = syn::parse2(tokens).expect("parse generated tokens");
        let content = prettyplease::unparse(&ast);

        let out = out_dir.join(format!("{flavor}_codegen.rs"));
        fs::write(&out, content).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
    }
}
