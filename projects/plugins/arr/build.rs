//! Generate typed clients for every *arr spec under `specs/`. One spec →
//! one module included by `src/lib.rs`. Modules are wired up in lib.rs via
//! a parallel list — add a new spec *and* a `flavor!(name)` line to enable
//! a flavor.
//!
//! All "make this upstream spec digestible by progenitor" work lives in
//! `openapi::normalize` — never patch a spec here.
//!
//! Codegen plumbing is centralised in `orca-plugin-toolkit-build` per
//! [[feedback-plugin-toolkit-is-the-gateway]].

fn main() {
    let specs_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("specs");
    plugin_toolkit_build::openapi::generate_all(specs_dir, "arr").expect("arr openapi codegen");
}
