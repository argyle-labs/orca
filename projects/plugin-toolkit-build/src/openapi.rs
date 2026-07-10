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
        utils::yaml::from_str(raw).with_context(|| format!("parse {flavor} as YAML"))
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
    generate_inner(
        specs_dir.as_ref(),
        plugin_tag,
        &[],
        CodegenOptions::default(),
    )
}

/// Opt-in adaptations for APIs whose wire representation diverges from the
/// vendored spec. Both default off, so [`generate_all`] is the zero-quirk path.
#[derive(Clone, Copy, Default)]
pub struct CodegenOptions<'a> {
    /// Fully-qualified path to a plugin `fn(serde_json::Value) -> Option<serde_json::Value>`
    /// that peels each response body before the typed types deserialize it (e.g.
    /// Proxmox's `{"data": …}` envelope). See [`generate_all_with_options`].
    pub unwrapper: Option<&'a str>,
    /// Anchor `::plugin_toolkit::serde_ext::{bool_lenient, opt_bool_lenient}` on
    /// every generated `bool` / `Option<bool>` field, so booleans documented as
    /// such but serialized as integer `0`/`1` (Proxmox VE) still deserialize.
    pub lenient_booleans: bool,
    /// Anchor `::plugin_toolkit::serde_ext::{f64_lenient, opt_f64_lenient}` on
    /// every generated `f64` / `Option<f64>` field, so numbers documented as
    /// `number` but serialized as quoted strings (`"0.00"` — Proxmox VE's PSI
    /// `pressure*` fields) still deserialize. The type stays `f64`.
    pub lenient_numbers: bool,
}

/// Like [`generate_all`], but applies the [`CodegenOptions`] wire-adaptation
/// quirks — for APIs whose live wire body diverges from the vendored spec.
///
/// - `unwrapper`: rather than bake one envelope convention into core, the plugin
///   exposes *how it unwraps* — a `fn(serde_json::Value) -> Option<serde_json::Value>`
///   whose path the injected `exec` hook hands to
///   [`plugin_toolkit::api_client::exec_with_unwrapper`]. Peels Proxmox's
///   `{"data": …}` (and any single- or multi-key envelope) at the transport
///   layer so the generated types stay the plain inner shape.
/// - `lenient_booleans`: keep fields the docs declare `boolean` typed as `bool`
///   while still accepting the integer `0`/`1` many APIs actually serialize.
///
/// Both leave the generated types matching the documented schema; only the
/// deserialize/transport seam adapts. No plugin call site touches either quirk.
pub fn generate_all_with_options(
    specs_dir: impl AsRef<Path>,
    plugin_tag: &str,
    options: CodegenOptions<'_>,
) -> Result<()> {
    generate_inner(specs_dir.as_ref(), plugin_tag, &[], options)
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
    generate_inner(
        specs_dir.as_ref(),
        plugin_tag,
        keep,
        CodegenOptions::default(),
    )
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
    let content = codegen_one(&raw, flavor, plugin_tag, keep, CodegenOptions::default())?;
    let out = out_dir.join(format!("{flavor}_codegen.rs"));
    fs::write(&out, content).with_context(|| format!("write {}", out.display()))?;
    Ok(())
}

fn generate_inner(
    specs_dir: &Path,
    plugin_tag: &str,
    keep: &[(&str, &[&str])],
    options: CodegenOptions<'_>,
) -> Result<()> {
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

        let content = codegen_one(&raw, flavor, plugin_tag, keep_paths, options)?;
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
    options: CodegenOptions<'_>,
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
    // Anchor serde derives to the toolkit's serde *before* unparsing: progenitor
    // emits `#[derive(::serde::Serialize, …)]` with no `#[serde(crate = …)]`, so
    // the derive macro would emit `::serde::*` impl paths and the plugin would
    // need a direct serde dep. (Same fix as the GraphQL codegen.)
    anchor_serde_derives(&mut ast.items);
    // Anchor lenient bool deserializers on `bool` / `Option<bool>` fields when
    // the plugin opted in — for APIs (Proxmox VE) that document booleans but
    // serialize integer 0/1. Runs on the AST before unparse so the attribute is
    // rendered by prettyplease alongside the field.
    if options.lenient_booleans {
        let n = anchor_lenient_bools(&mut ast.items);
        println!(
            "cargo:warning={plugin_tag}::{flavor}: anchored lenient bool deserializer on {n} field(s)"
        );
    }
    if options.lenient_numbers {
        let n = anchor_lenient_numbers(&mut ast.items);
        println!(
            "cargo:warning={plugin_tag}::{flavor}: anchored lenient number deserializer on {n} field(s)"
        );
    }
    let src = rewrite_codegen_paths(&prettyplease::unparse(&ast));
    Ok(match options.unwrapper {
        Some(path) => inject_exec_unwrapper(src, path, plugin_tag, flavor),
        None => src,
    })
}

/// Anchor `::plugin_toolkit::serde_ext::{bool_lenient, opt_bool_lenient}` on
/// every `bool` / `Option<bool>` struct field, recursing into the generated
/// module tree. Returns the number of fields touched. Mirrors
/// [`anchor_serde_derives`]. A field that already carries `deserialize_with` is
/// left alone, so this is idempotent and never fights a hand-authored override.
fn anchor_lenient_bools(items: &mut [syn::Item]) -> usize {
    let mut count = 0;
    for item in items.iter_mut() {
        match item {
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = m.content.as_mut() {
                    count += anchor_lenient_bools(inner);
                }
            }
            syn::Item::Struct(s) => {
                for field in s.fields.iter_mut() {
                    if let Some(with) = lenient_bool_fn(&field.ty)
                        && !has_deserialize_with(&field.attrs)
                    {
                        // Only `deserialize_with`: progenitor already emits
                        // `#[serde(default, …)]` on optional fields, so adding
                        // `default` here would duplicate it; a required `bool`
                        // keeps its original "key must be present" contract.
                        let attr: syn::Attribute = syn::parse_quote!(
                            #[serde(deserialize_with = #with)]
                        );
                        field.attrs.push(attr);
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// The lenient-deserializer path for a field type, or `None` if it isn't a
/// bare `bool` or `Option<bool>`.
fn lenient_bool_fn(ty: &syn::Type) -> Option<&'static str> {
    if is_ident(ty, "bool") {
        return Some("::plugin_toolkit::serde_ext::bool_lenient");
    }
    if let Some(inner) = option_inner(ty)
        && is_ident(inner, "bool")
    {
        return Some("::plugin_toolkit::serde_ext::opt_bool_lenient");
    }
    None
}

/// Anchor `::plugin_toolkit::serde_ext::{f64_lenient, opt_f64_lenient}` on every
/// `f64` / `Option<f64>` struct field, recursing into the generated module tree.
/// Returns the number of fields touched. Mirrors [`anchor_lenient_bools`]; a
/// field that already carries `deserialize_with` is left alone (idempotent).
fn anchor_lenient_numbers(items: &mut [syn::Item]) -> usize {
    let mut count = 0;
    for item in items.iter_mut() {
        match item {
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = m.content.as_mut() {
                    count += anchor_lenient_numbers(inner);
                }
            }
            syn::Item::Struct(s) => {
                for field in s.fields.iter_mut() {
                    if let Some(with) = lenient_number_fn(&field.ty)
                        && !has_deserialize_with(&field.attrs)
                    {
                        let attr: syn::Attribute = syn::parse_quote!(
                            #[serde(deserialize_with = #with)]
                        );
                        field.attrs.push(attr);
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }
    count
}

/// The lenient-deserializer path for a field type, or `None` if it isn't a
/// bare `f64` or `Option<f64>`.
fn lenient_number_fn(ty: &syn::Type) -> Option<&'static str> {
    if is_ident(ty, "f64") {
        return Some("::plugin_toolkit::serde_ext::f64_lenient");
    }
    if let Some(inner) = option_inner(ty)
        && is_ident(inner, "f64")
    {
        return Some("::plugin_toolkit::serde_ext::opt_f64_lenient");
    }
    None
}

/// True if `ty` is a path ending in the single segment `ident` (e.g. `bool`).
fn is_ident(ty: &syn::Type, ident: &str) -> bool {
    matches!(ty, syn::Type::Path(p)
        if p.qself.is_none()
            && p.path.segments.last().is_some_and(|s| s.ident == ident
                && matches!(s.arguments, syn::PathArguments::None)))
}

/// If `ty` is `Option<T>` (however the path is spelled), return `T`.
fn option_inner(ty: &syn::Type) -> Option<&syn::Type> {
    let syn::Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// True if the field already sets a serde `deserialize_with`.
fn has_deserialize_with(attrs: &[syn::Attribute]) -> bool {
    use quote::ToTokens;
    attrs.iter().any(|a| {
        a.path().is_ident("serde") && a.to_token_stream().to_string().contains("deserialize_with")
    })
}

/// Rewire the generated client's `exec` hook to a plugin-supplied unwrapper.
///
/// progenitor emits an empty default `impl ClientHooks<()> for &Client {}`,
/// whose default `exec` just runs the request unchanged. When the plugin
/// declares an unwrapper we replace that empty impl with an `exec` override that
/// routes every response through
/// [`plugin_toolkit::api_client::exec_with_unwrapper`], handing it the plugin's
/// `unwrapper_path` — a pure `fn(serde_json::Value) -> Option<serde_json::Value>`
/// that peels the body (Proxmox's `{"data": …}`, etc.) before the typed types
/// deserialize it. A client with no declared unwrapper keeps the empty default,
/// so bodies pass through untouched — "if a client has an unwrapper use it,
/// otherwise don't." String-level rather than AST: the emitted impl is a fixed,
/// argument-free token sequence, so an exact-text swap is unambiguous. The
/// override names only `::plugin_toolkit::*` paths plus the trait items the
/// generated file already `use`s, so progenitor stays fully behind the toolkit.
fn inject_exec_unwrapper(
    src: String,
    unwrapper_path: &str,
    plugin_tag: &str,
    flavor: &str,
) -> String {
    const EMPTY_IMPL: &str = "impl ClientHooks<()> for &Client {}";
    let override_impl = format!(
        "impl ClientHooks<()> for &Client {{\n    \
         #[allow(clippy::manual_async_fn)]\n    \
         async fn exec(\n        &self,\n        \
         request: ::plugin_toolkit::reqwest::Request,\n        \
         _info: &OperationInfo,\n    ) -> \
         ::plugin_toolkit::reqwest::Result<::plugin_toolkit::reqwest::Response> {{\n        \
         ::plugin_toolkit::api_client::exec_with_unwrapper(self.client(), request, {unwrapper_path}).await\n    }}\n}}"
    );
    if let Some(idx) = src.find(EMPTY_IMPL) {
        let mut out = String::with_capacity(src.len() + override_impl.len());
        out.push_str(&src[..idx]);
        out.push_str(&override_impl);
        out.push_str(&src[idx + EMPTY_IMPL.len()..]);
        out
    } else {
        println!(
            "cargo:warning={plugin_tag}::{flavor}: unwrapper requested but the empty \
             `impl ClientHooks<()> for &Client {{}}` was not found in generated output; \
             response bodies will NOT be unwrapped"
        );
        src
    }
}

/// Add `#[serde(crate = "::plugin_toolkit::serde")]` to every struct/enum that
/// derives a serde trait and doesn't already set the crate, recursing into the
/// generated module tree.
fn anchor_serde_derives(items: &mut [syn::Item]) {
    for item in items.iter_mut() {
        match item {
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = m.content.as_mut() {
                    anchor_serde_derives(inner);
                }
            }
            syn::Item::Struct(s) => anchor_attrs(&mut s.attrs),
            syn::Item::Enum(e) => anchor_attrs(&mut e.attrs),
            _ => {}
        }
    }
}

fn anchor_attrs(attrs: &mut Vec<syn::Attribute>) {
    use quote::ToTokens;
    let derives_serde = attrs
        .iter()
        .any(|a| a.path().is_ident("derive") && a.to_token_stream().to_string().contains("serde"));
    let already_anchored = attrs
        .iter()
        .any(|a| a.path().is_ident("serde") && a.to_token_stream().to_string().contains("crate"));
    if derives_serde && !already_anchored {
        attrs.push(syn::parse_quote!(#[serde(crate = "::plugin_toolkit::serde")]));
    }
}

/// Redirect the crate-root paths progenitor emits so they resolve through the
/// toolkit re-exports — an OpenAPI plugin then needs no direct dep on serde,
/// serde_json, reqwest, progenitor_client, regress, chrono, uuid, bytes, or
/// futures_core. progenitor emits fully-qualified `::serde::…` plus the bare
/// `progenitor_client::…` of its prelude `use`; both are rewritten here.
fn rewrite_codegen_paths(s: &str) -> String {
    // Order matters: redirect `serde_json` before `serde` so the shorter rule
    // can't be tempted by the longer name (the segment-boundary `::` already
    // prevents it, but keep it explicit).
    const CRATES: &[&str] = &[
        "serde_json",
        "serde",
        "reqwest",
        "progenitor_client",
        "regress",
        "chrono",
        "uuid",
        "bytes",
        "futures_core",
    ];
    let mut out = s.to_string();
    for krate in CRATES {
        out = redirect_crate(&out, krate);
    }
    out
}

/// Rewrite `<krate>::…` and `::<krate>::…` to `::plugin_toolkit::<krate>::…`.
/// prettyplease renders paths without spaces (`::serde::`), so the patterns are
/// the bare-name form; a sentinel guards the already-absolute occurrence so it
/// is not double-prefixed. The trailing `::` anchors a segment boundary, so
/// `serde_json` never matches the `serde` rule.
fn redirect_crate(s: &str, krate: &str) -> String {
    let abs = format!("::{krate}::");
    let bare = format!("{krate}::");
    let target = format!("::plugin_toolkit::{krate}::");
    let sentinel = format!("\u{0}{krate}\u{0}");
    let s = s.replace(&abs, &sentinel);
    let s = s.replace(&bare, &target);
    s.replace(&sentinel, &target)
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
