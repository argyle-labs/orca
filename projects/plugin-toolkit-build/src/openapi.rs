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
//!
//! [`generate_all`] codegens the whole spec. [`generate_selected`] first
//! prunes each spec to a per-flavor keep-list of paths (plus the transitive
//! `$ref` closure of the schemas they touch) — use it for large upstream
//! specs where progenitor on the full document would emit hundreds of unused
//! types. See [`crate::prune`].

// `Value` models the raw upstream OpenAPI tree (possibly 3.1 pre-lowering);
// there is no fixed struct to deserialize into at this stage. Same stance as
// `openapi::lower_31` and `crate::prune`.
#![allow(clippy::disallowed_types)]

use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::value::Value;

/// Spec filename suffixes the codegen pipeline accepts. The basename minus the
/// matched suffix becomes the flavor (module) name.
const SPEC_SUFFIXES: &[&str] = &[".openapi.json", ".openapi.yaml", ".openapi.yml"];

/// Parse raw spec text (JSON or YAML) into the raw OpenAPI value tree.
/// Detection mirrors `openapi::parse_str`: a document starting with `{` is
/// JSON, everything else is YAML. YAML maps cleanly onto the same value model,
/// so a 3.1 YAML spec rides the same lower -> prune -> normalize path as JSON.
fn parse_spec_value(raw: &str, flavor: &str) -> Result<Value> {
    if raw.trim_start().starts_with('{') {
        serde_json::from_str(raw).with_context(|| format!("parse {flavor} as JSON"))
    } else {
        serde_yaml::from_str(raw).with_context(|| format!("parse {flavor} as YAML"))
    }
}

/// Strip a recognized spec suffix from a filename, returning the flavor name.
fn flavor_of(file_name: &str) -> Option<&str> {
    SPEC_SUFFIXES
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))
}

/// Generate typed clients for every `*.openapi.json` spec under `specs_dir`.
///
/// `plugin_tag` is prepended to cargo warning lines emitted by the
/// normalize pass — typically the plugin name (e.g. `"arr"`).
///
/// Each spec produces `<OUT_DIR>/<flavor>_codegen.rs`. Caller `include!`s
/// these files from `src/lib.rs`.
pub fn generate_all(specs_dir: impl AsRef<Path>, plugin_tag: &str) -> Result<()> {
    generate_inner(specs_dir.as_ref(), plugin_tag, &[])
}

/// Like [`generate_all`], but prune each named flavor to a keep-list of paths
/// before codegen. `keep` maps `flavor -> &[path]`; a flavor absent from
/// `keep` is codegenned whole (same as [`generate_all`]). Pruning runs *after*
/// any 3.1→3.0 lowering so the keep-list is matched against the final path set.
///
/// ```rust,ignore
/// generate_selected("specs", "jellyfin", &[
///     ("jellyfin", &["/System/Info", "/Sessions", "/Library/VirtualFolders"]),
/// ])?;
/// ```
pub fn generate_selected(
    specs_dir: impl AsRef<Path>,
    plugin_tag: &str,
    keep: &[(&str, &[&str])],
) -> Result<()> {
    generate_inner(specs_dir.as_ref(), plugin_tag, keep)
}

/// Codegen a single spec file under an explicit `flavor` module name,
/// pruning to `keep_paths` (empty = whole spec). Use this when the vendored
/// spec filename does not follow the `<flavor>.openapi.json` convention —
/// e.g. an upstream-versioned `jellyfin-openapi-12.0.0.json` — so the file can
/// stay named as published while still emitting a clean `<flavor>` module.
///
/// Produces `<OUT_DIR>/<flavor>_codegen.rs`.
pub fn generate_one(
    spec_path: impl AsRef<Path>,
    flavor: &str,
    plugin_tag: &str,
    keep_paths: &[&str],
) -> Result<()> {
    let spec_path = spec_path.as_ref();
    let out_dir = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .context("OUT_DIR not set — generate_one must be called from build.rs")?;
    println!("cargo:rerun-if-changed={}", spec_path.display());

    let raw =
        fs::read_to_string(spec_path).with_context(|| format!("read {}", spec_path.display()))?;
    let keep = (!keep_paths.is_empty()).then_some(keep_paths);
    let content = codegen_one(&raw, flavor, plugin_tag, keep)?;
    let out = out_dir.join(format!("{flavor}_codegen.rs"));
    fs::write(&out, content).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

fn generate_inner(specs_dir: &Path, plugin_tag: &str, keep: &[(&str, &[&str])]) -> Result<()> {
    let out_dir = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .context("OUT_DIR not set — generate_* must be called from build.rs")?;

    println!("cargo:rerun-if-changed={}", specs_dir.display());

    let mut entries: Vec<_> = fs::read_dir(specs_dir)
        .with_context(|| format!("read {}", specs_dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| flavor_of(n).is_some())
        })
        .collect();
    entries.sort_by_key(|e| e.path());

    for entry in entries {
        let spec_path = entry.path();
        println!("cargo:rerun-if-changed={}", spec_path.display());

        let flavor = spec_path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(flavor_of)
            .with_context(|| {
                format!(
                    "spec filename must be <flavor>.openapi.{{json,yaml,yml}}: {}",
                    spec_path.display()
                )
            })?;

        let raw = fs::read_to_string(&spec_path)
            .with_context(|| format!("read {}", spec_path.display()))?;
        let keep_paths = keep
            .iter()
            .find(|(f, _)| *f == flavor)
            .map(|(_, paths)| *paths);

        let content = codegen_one(&raw, flavor, plugin_tag, keep_paths)?;
        let out = out_dir.join(format!("{flavor}_codegen.rs"));
        fs::write(&out, content).with_context(|| format!("write {}", out.display()))?;
    }
    Ok(())
}

/// Run the full lower → prune → normalize → progenitor pipeline for one spec
/// and return the formatted Rust source.
fn codegen_one(
    raw: &str,
    flavor: &str,
    plugin_tag: &str,
    keep_paths: Option<&[&str]>,
) -> Result<String> {
    // Parse to a raw JSON value first so we can detect the spec version.
    // `openapiv3` models OpenAPI 3.0 and fails on 3.1-only constructs
    // (`type: [..,"null"]`, numeric `exclusiveMinimum`, …), so a 3.1
    // document must be lowered to 3.0 *before* it can deserialize into
    // `openapiv3::OpenAPI`. 3.0 specs pass straight through unchanged.
    // Raw JSON value: the input is, by construction, not yet a valid
    // `openapiv3` document (it may be 3.1), so there is no typed struct to
    // deserialize into at this stage — the lowering pass rewrites the
    // open-ended upstream tree before it becomes typed.
    let mut value: Value = parse_spec_value(raw, flavor)?;

    if openapi::lower_31::is_31(&value) {
        let lowering = openapi::lower_31::lower_to_30(&mut value)
            .with_context(|| format!("lower 3.1 -> 3.0 for {flavor}"))?;
        lowering.emit_cargo_warnings(&format!("{plugin_tag}::{flavor}"));
    }

    if let Some(paths) = keep_paths {
        let kept = crate::prune::to_paths(&mut value, paths)
            .with_context(|| format!("prune {flavor} to keep-list"))?;
        println!(
            "cargo:warning={plugin_tag}::{flavor}: pruned to {} path(s), {} schema(s) retained",
            paths.len(),
            kept.len()
        );
    }

    let mut spec: openapiv3::OpenAPI = serde_json::value::from_value(value)
        .with_context(|| format!("parse {flavor} as openapiv3"))?;

    let report = openapi::normalize::for_progenitor(&mut spec);
    report.emit_cargo_warnings(&format!("{plugin_tag}::{flavor}"));

    let tokens = progenitor::Generator::default()
        .generate_tokens(&spec)
        .with_context(|| format!("progenitor codegen for {flavor}"))?;
    let mut ast = syn::parse2(tokens).context("parse generated tokens")?;
    for (st, old, new) in dedupe_struct_fields(&mut ast) {
        println!(
            "cargo:warning={plugin_tag}::{flavor}: renamed duplicate field {st}.{old} -> {new} (wire key preserved)"
        );
    }
    Ok(prettyplease::unparse(&ast))
}

/// progenitor/typify sanitize OpenAPI property names to snake_case Rust idents,
/// so two distinct wire keys (e.g. `Guid` and `guid`, or `Rating` and `rating`)
/// collapse to the same identifier and produce a struct with a duplicate field —
/// which does not compile. Rather than drop a field or rename a wire key (both
/// lossy), rename only the *later* colliding Rust ident to `<ident>_<n>` and,
/// if it has no `#[serde(rename = ...)]`, attach one carrying its original wire
/// key. Both fields survive and (de)serialize against their true wire names.
///
/// Returns `(struct, old_ident, new_ident)` for each rename, for cargo warnings.
fn dedupe_struct_fields(file: &mut syn::File) -> Vec<(String, String, String)> {
    let mut renames = Vec::new();
    dedupe_items(&mut file.items, &mut renames);
    renames
}

fn dedupe_items(items: &mut [syn::Item], renames: &mut Vec<(String, String, String)>) {
    for item in items.iter_mut() {
        match item {
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = m.content.as_mut() {
                    dedupe_items(inner, renames);
                }
            }
            syn::Item::Struct(s) => dedupe_struct(s, renames),
            _ => {}
        }
    }
}

fn dedupe_struct(s: &mut syn::ItemStruct, renames: &mut Vec<(String, String, String)>) {
    let syn::Fields::Named(named) = &mut s.fields else {
        return;
    };
    let struct_name = s.ident.to_string();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for field in named.named.iter_mut() {
        let Some(ident) = field.ident.clone() else {
            continue;
        };
        let base = ident.to_string();
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            continue;
        }
        // Duplicate ident: keep the first, rename this one and pin its wire key.
        if !field_has_serde_rename(&field.attrs) {
            field.attrs.push(make_serde_rename(&base));
        }
        let new_name = format!("{base}_{count}");
        field.ident = Some(syn::Ident::new(&new_name, ident.span()));
        renames.push((struct_name.clone(), base, new_name));
    }
}

fn field_has_serde_rename(attrs: &[syn::Attribute]) -> bool {
    use quote::ToTokens;
    attrs
        .iter()
        .any(|a| a.path().is_ident("serde") && a.to_token_stream().to_string().contains("rename"))
}

fn make_serde_rename(wire: &str) -> syn::Attribute {
    let lit = syn::LitStr::new(wire, proc_macro2::Span::call_site());
    syn::parse_quote!(#[serde(rename = #lit)])
}
