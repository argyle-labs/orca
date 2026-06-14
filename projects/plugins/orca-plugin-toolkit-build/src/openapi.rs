//! OpenAPI codegen helper for plugin `build.rs` files.
//!
//! Walks `<dir>/<flavor>.openapi.json`, runs the shared normalize pass
//! ([`openapi::normalize::for_progenitor`]), runs progenitor, and writes
//! `<flavor>_codegen.rs` into `OUT_DIR`. The plugin then `include!`s each
//! emitted file from its `src/lib.rs`.
//!
//! Drop a new spec file in → next build emits a new module.
//!
//! Filename convention: `<flavor>.openapi.json`. The flavor (basename
//! minus suffix) becomes both the module ident and the codegen filename.

use std::{ffi::OsStr, fs, path::Path};

use anyhow::{Context, Result};

/// Generate typed clients for every `*.openapi.json` spec under `specs_dir`.
///
/// `plugin_tag` is prepended to cargo warning lines emitted by the
/// normalize pass — typically the plugin name (e.g. `"arr"`).
///
/// Each spec produces `<OUT_DIR>/<flavor>_codegen.rs`. Caller `include!`s
/// these files from `src/lib.rs`.
pub fn generate_all(specs_dir: impl AsRef<Path>, plugin_tag: &str) -> Result<()> {
    let specs_dir = specs_dir.as_ref();
    let out_dir = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .context("OUT_DIR not set — generate_all must be called from build.rs")?;

    println!("cargo:rerun-if-changed={}", specs_dir.display());

    let mut entries: Vec<_> = fs::read_dir(specs_dir)
        .with_context(|| format!("read {}", specs_dir.display()))?
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
            .with_context(|| {
                format!(
                    "spec filename must be <flavor>.openapi.json: {}",
                    spec_path.display()
                )
            })?;

        let raw = fs::read_to_string(&spec_path)
            .with_context(|| format!("read {}", spec_path.display()))?;
        let mut spec: openapiv3::OpenAPI = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", spec_path.display()))?;

        let report = openapi::normalize::for_progenitor(&mut spec);
        report.emit_cargo_warnings(&format!("{plugin_tag}::{flavor}"));

        let tokens = progenitor::Generator::default()
            .generate_tokens(&spec)
            .with_context(|| format!("progenitor codegen for {flavor}"))?;
        let ast = syn::parse2(tokens).context("parse generated tokens")?;
        let content = prettyplease::unparse(&ast);

        let out = out_dir.join(format!("{flavor}_codegen.rs"));
        fs::write(&out, content).with_context(|| format!("write {}", out.display()))?;
    }
    Ok(())
}
