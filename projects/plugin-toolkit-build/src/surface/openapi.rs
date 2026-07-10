//! OpenAPI → orca tool-surface generator.
//!
//! Runs in a plugin `build.rs` *after* [`crate::openapi::generate_all*`] has
//! written `<out_dir>/<flavor>_codegen.rs`. Rather than hand-write one
//! `#[orca_tool]` per capability, this pairs every generated `impl Client`
//! method back to its spec operation via progenitor's doc comment
//! (`Sends a `GET` request to `/nodes/{node}/tasks/{upid}/status``), applies a
//! declarative ruleset, and emits:
//!
//! It writes `<flavor>_surface.rs` — an `#[orca_tool]` wrapper per matched
//! method, with an args struct carrying the method's params (including the full
//! typed request body) and a body that calls the generated method through
//! `crate::tools::make_client`. It also anchors JsonSchema derives onto the
//! transitive closure of every type the surfaced tools reference — request
//! bodies, query enums, and response bodies — so the complete request/response
//! shape is known at runtime via the tool's `args_schema` / `output_schema`.
//!
//! `OrcaToolDef::Args` requires only `DeserializeOwned + Serialize + JsonSchema`
//! (NOT `clap::Args`) — the CLI surface is generated from the JSON Schema — so a
//! nested typed body field is a first-class arg, not a JSON blob.
//!
//! Write methods (POST/PUT/DELETE) are emitted `data_mutation = true` +
//! `role = "admin"`. A specific write can be made user-callable without the
//! `can_mutate` opt-in by setting `x-orca-user-callable: true` on the operation
//! in the spec — it then keeps `data_mutation = true` but is emitted
//! `role = "read"`.

#![allow(clippy::disallowed_types)]

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use quote::ToTokens;
use regex::Regex;

use crate::surface::common::{
    GENERATED_HEADER, collect_doc, first_generic, is_ident, option_inner, push_jsonschema_anchor,
};

/// Spec filename suffixes scanned for the `x-orca-user-callable` exception.
const SPEC_SUFFIXES: &[&str] = &[".openapi.json", ".openapi.yaml", ".openapi.yml"];

/// A generated `impl Client` method paired to its spec operation.
struct Method {
    ident: String,
    http: String,
    path: String,
    params: Vec<Param>,
    /// The `T` inside `ResponseValue<T>`, as source text with `types::` paths
    /// rewritten to `crate::generated::types::`. `None` for `()` / unit.
    ret: Option<String>,
    /// Bare `types::*` idents referenced by params + return — closure seeds.
    type_seeds: Vec<String>,
}

/// A single surfaceable method argument, with its emitted parts precomputed.
struct Param {
    /// Field declaration line inside the args struct (no trailing comma).
    field_decl: String,
    /// Expression passed at the call site, in the method's positional order.
    call_expr: String,
}

/// One surface rule: match on `"<METHOD> <path>"`, name a verb prefix.
///
/// Every rule derives its verb from the method ident (a unique, deterministic
/// name); there is no per-rule verb override.
struct Rule {
    re: Regex,
    role_admin: bool,
}

/// The single place that decides what becomes orca surface.
///
/// Surface **everything emittable** with full typed request + response bodies.
/// Mutating methods (POST/PUT/DELETE) get `role = "admin"` + `data_mutation`.
/// Verb = the generated method ident (unique, deterministic).
fn rules() -> Vec<Rule> {
    vec![
        Rule {
            re: Regex::new(r"^(POST|PUT|DELETE) ").unwrap(),
            role_admin: true,
        },
        Rule {
            re: Regex::new(r"^GET ").unwrap(),
            role_admin: false,
        },
    ]
}

/// Generate `<flavor>_surface.rs` from the codegen'd `<flavor>_codegen.rs`.
///
/// - `specs_dir`: where the `<flavor>.openapi.{json,yaml,yml}` spec(s) live —
///   scanned for the `x-orca-user-callable` per-operation exception.
/// - `out_dir`: the codegen output dir (`OUT_DIR`). Reads
///   `<out_dir>/<flavor>_codegen.rs`, rewrites it in place to anchor
///   JsonSchema, and writes `<out_dir>/<flavor>_surface.rs`.
/// - `flavor`: the module/domain name (`"proxmox"`). Becomes the tool domain.
pub fn generate(specs_dir: &Path, out_dir: &Path, flavor: &str) -> Result<()> {
    println!("cargo:rerun-if-changed={}", specs_dir.display());
    let gen_path = out_dir.join(format!("{flavor}_codegen.rs"));
    let src = std::fs::read_to_string(&gen_path)
        .with_context(|| format!("read {}", gen_path.display()))?;
    let mut file: syn::File = syn::parse_file(&src).context("parse generated codegen")?;

    let exceptions = user_callable_exceptions(specs_dir)?;

    let type_idents = collect_type_idents(&file);
    let methods = collect_methods(&file, &type_idents);
    let rules = rules();

    let mut matched: Vec<(&Method, bool)> = Vec::new();
    let mut skipped = 0usize;
    for m in &methods {
        let key = format!("{} {}", m.http, m.path);
        let Some(r) = rules.iter().find(|r| r.re.is_match(&key)) else {
            continue;
        };
        if m.ret.is_none() {
            skipped += 1;
            continue;
        }
        matched.push((m, r.role_admin));
    }

    // Transitive closure of every type the surfaced tools touch, then anchor
    // JsonSchema so the full request/response shape is runtime-introspectable.
    let field_refs = collect_type_field_refs(&file, &type_idents);
    let mut needed: BTreeSet<String> = BTreeSet::new();
    for (m, _) in &matched {
        for seed in &m.type_seeds {
            close_over(seed, &field_refs, &mut needed);
        }
    }
    let anchored = anchor_jsonschema(&mut file, &needed);
    std::fs::write(&gen_path, prettyplease::unparse(&file))
        .with_context(|| format!("rewrite {}", gen_path.display()))?;

    let exception_hits = matched
        .iter()
        .filter(|(m, role_admin)| {
            *role_admin && exceptions.contains(&(m.http.clone(), m.path.clone()))
        })
        .count();
    println!(
        "cargo:warning=surface[{flavor}]: {} tool(s) emitted, {skipped} skipped (unit return), \
         JsonSchema on {anchored}/{} type(s), {exception_hits} user-callable exception(s)",
        matched.len(),
        needed.len()
    );

    let surface = emit_surface(&matched, flavor, &exceptions);
    let surface_path = out_dir.join(format!("{flavor}_surface.rs"));
    std::fs::write(&surface_path, surface)
        .with_context(|| format!("write {}", surface_path.display()))?;
    Ok(())
}

/// `(METHOD_UPPER, path)` for every operation marked `x-orca-user-callable: true`
/// across the spec file(s) in `specs_dir`. Reads the raw spec value (JSON/YAML)
/// so the vendor extension is seen regardless of 3.1→3.0 lowering.
fn user_callable_exceptions(specs_dir: &Path) -> Result<HashSet<(String, String)>> {
    let mut out = HashSet::new();
    let Ok(rd) = std::fs::read_dir(specs_dir) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !SPEC_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            continue;
        }
        let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let value: serde_json::Value = if raw.trim_start().starts_with('{') {
            serde_json::from_str(&raw).with_context(|| format!("parse {name} as JSON"))?
        } else {
            utils::yaml::from_str(&raw).with_context(|| format!("parse {name} as YAML"))?
        };
        exceptions_from_spec_value(&value, &mut out);
    }
    Ok(out)
}

/// Scan an OpenAPI value for operations flagged `x-orca-user-callable: true`,
/// inserting `(METHOD_UPPER, path)` into `out`. Split out for unit testing.
fn exceptions_from_spec_value(v: &serde_json::Value, out: &mut HashSet<(String, String)>) {
    let Some(paths) = v.get("paths").and_then(|p| p.as_object()) else {
        return;
    };
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (method, op) in item {
            let m = method.to_ascii_uppercase();
            if !matches!(m.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                continue;
            }
            if op.get("x-orca-user-callable").and_then(|x| x.as_bool()) == Some(true) {
                out.insert((m, path.clone()));
            }
        }
    }
}

/// Walk every `impl Client` block and turn each surfaceable `pub async fn` into
/// a [`Method`]. A method using an arg/return shape the emitter can't render is
/// dropped (returns `None` from [`method_from_fn`]).
fn collect_methods(file: &syn::File, locals: &BTreeSet<String>) -> Vec<Method> {
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut drops: std::collections::BTreeMap<String, usize> = Default::default();
    for item in &file.items {
        let syn::Item::Impl(imp) = item else { continue };
        let is_client = matches!(&*imp.self_ty, syn::Type::Path(p)
            if p.path.segments.last().is_some_and(|s| s.ident == "Client"));
        if !is_client {
            continue;
        }
        for ii in &imp.items {
            let syn::ImplItem::Fn(f) = ii else { continue };
            total += 1;
            match method_from_fn(f, locals) {
                Ok(m) => out.push(m),
                Err(reason) => *drops.entry(reason.into()).or_default() += 1,
            }
        }
    }
    let dropped: usize = drops.values().sum();
    println!(
        "cargo:warning=surface: paired {}/{total} client methods ({dropped} dropped: {})",
        out.len(),
        drops
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    out
}

fn method_from_fn(f: &syn::ImplItemFn, locals: &BTreeSet<String>) -> Result<Method, &'static str> {
    let doc = collect_doc(&f.attrs);
    let re = Regex::new(r"Sends a `(GET|POST|PUT|DELETE|PATCH)` request to `([^`]+)`").unwrap();
    let caps = re.captures(&doc).ok_or("no-doc-path")?;
    let http = caps[1].to_string();
    let path = caps[2].to_string();

    let mut params = Vec::new();
    let mut seeds = Vec::new();
    for arg in &f.sig.inputs {
        let syn::FnArg::Typed(pt) = arg else { continue };
        let syn::Pat::Ident(pi) = &*pt.pat else {
            return Err("non-ident-param");
        };
        let name = pi.ident.to_string();
        let (param, mut param_seeds) = classify(&name, &pt.ty, locals).ok_or("param-type")?;
        params.push(param);
        seeds.append(&mut param_seeds);
    }

    let (ret, mut ret_seeds) = return_inner(&f.sig.output, locals).ok_or("return-type")?;
    seeds.append(&mut ret_seeds);
    Ok(Method {
        ident: f.sig.ident.to_string(),
        http,
        path,
        params,
        ret,
        type_seeds: seeds,
    })
}

/// Classify one method param into an emitted [`Param`] + the `types::*` idents
/// it seeds into the JsonSchema closure. `None` if the shape isn't emittable.
fn classify(name: &str, ty: &syn::Type, locals: &BTreeSet<String>) -> Option<(Param, Vec<String>)> {
    // `&str` / `&'a str` path param → `String`, passed by ref.
    if let syn::Type::Reference(r) = ty
        && is_ident(&r.elem, "str")
    {
        return Some((field(name, "String", &format!("&args.{name}")), vec![]));
    }
    // `&Body` typed request body → nested typed field, passed by ref.
    if let syn::Type::Reference(r) = ty
        && let Some(rendered) = rendered_local_type(&r.elem, locals)
    {
        let mut seeds = Vec::new();
        collect_types_idents_in_ty(&r.elem, &mut seeds);
        return Some((field(name, &rendered, &format!("&args.{name}")), seeds));
    }
    // bare scalar path param (e.g. `vmid: i64`) → same type, by value.
    for scalar in SCALARS {
        if is_ident(ty, scalar) {
            return Some((field(name, scalar, &format!("args.{name}")), vec![]));
        }
    }
    // `Option<...>` query params.
    if let Some(inner) = option_inner(ty) {
        // `Option<&str>` → `Option<String>`, `.as_deref()`.
        if let syn::Type::Reference(r) = inner
            && is_ident(&r.elem, "str")
        {
            return Some((
                field(name, "Option<String>", &format!("args.{name}.as_deref()")),
                vec![],
            ));
        }
        // `Option<&[String]>` / `Option<Vec<String>>` array query.
        if let Some(elem) = slice_or_vec_inner(inner)
            && is_ident(elem, "String")
        {
            let by_ref = matches!(inner, syn::Type::Reference(_));
            let call = if by_ref {
                format!("args.{name}.as_deref()")
            } else {
                format!("args.{name}")
            };
            return Some((field(name, "Option<Vec<String>>", &call), vec![]));
        }
        // `Option<scalar>` → same, by value.
        for scalar in SCALARS {
            if is_ident(inner, scalar) {
                return Some((
                    field(name, &format!("Option<{scalar}>"), &format!("args.{name}")),
                    vec![],
                ));
            }
        }
        // `Option<types::Enum>` query enum → keep the typed enum, by value.
        if let Some(rendered) = rendered_local_type(inner, locals) {
            let mut seeds = Vec::new();
            collect_types_idents_in_ty(inner, &mut seeds);
            return Some((
                field(
                    name,
                    &format!("Option<{rendered}>"),
                    &format!("args.{name}"),
                ),
                seeds,
            ));
        }
    }
    None
}

const SCALARS: &[&str] = &["u64", "i64", "u32", "i32", "u16", "f64", "bool"];

fn field(name: &str, ty: &str, call_expr: &str) -> Param {
    Param {
        field_decl: format!("    pub {name}: {ty},"),
        call_expr: call_expr.to_string(),
    }
}

/// If `ty` is a `types::Foo` path (a locally-defined generated type), render it
/// as `crate::generated::types::Foo`. Rejects non-local / primitive paths.
fn rendered_local_type(ty: &syn::Type, locals: &BTreeSet<String>) -> Option<String> {
    let syn::Type::Path(p) = ty else { return None };
    let last = p.path.segments.last()?;
    let starts_types = p.path.segments.first().is_some_and(|s| s.ident == "types");
    if !(starts_types || locals.contains(&last.ident.to_string())) {
        return None;
    }
    Some(rewrite_types_path(&ty.to_token_stream().to_string()))
}

/// Extract `T` from `Result<ResponseValue<T>, Error<...>>`, rewrite `types::`
/// paths, and collect its `types::*` seeds. `None` return means an unsurfaceable
/// output (byte streams, opaque JSON) → skip the method entirely. `Some(None)`
/// return means unit `()`.
fn return_inner(
    output: &syn::ReturnType,
    locals: &BTreeSet<String>,
) -> Option<(Option<String>, Vec<String>)> {
    let syn::ReturnType::Type(_, ty) = output else {
        return Some((None, vec![]));
    };
    let result_ok = first_generic(ty, "Result")?;
    let inner = first_generic(result_ok, "ResponseValue")?;
    // Unit response.
    if let syn::Type::Tuple(t) = inner
        && t.elems.is_empty()
    {
        return Some((None, vec![]));
    }
    if !return_is_surfaceable(inner, locals) {
        return None;
    }
    let mut seeds = Vec::new();
    collect_types_idents_in_ty(inner, &mut seeds);
    let rendered = rewrite_types_path(&inner.to_token_stream().to_string());
    Some((Some(rendered), seeds))
}

/// True if the return type is something we can hand to schemars: a local
/// `types::*`, a `Vec`/`Option` thereof, `String`, or a scalar. Byte streams and
/// opaque `serde_json::Value` are rejected.
fn return_is_surfaceable(ty: &syn::Type, locals: &BTreeSet<String>) -> bool {
    match ty {
        syn::Type::Path(p) => {
            let Some(last) = p.path.segments.last() else {
                return false;
            };
            let id = last.ident.to_string();
            let ok_leaf = id == "String"
                || id == "Vec"
                || id == "Option"
                || SCALARS.contains(&id.as_str())
                || p.path.segments.first().is_some_and(|s| s.ident == "types")
                || locals.contains(&id);
            if !ok_leaf {
                return false;
            }
            if let syn::PathArguments::AngleBracketed(a) = &last.arguments {
                for arg in &a.args {
                    if let syn::GenericArgument::Type(t) = arg
                        && !return_is_surfaceable(t, locals)
                    {
                        return false;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// Emit the surface source: header + one `#[orca_tool]` block per matched method.
fn emit_surface(
    matched: &[(&Method, bool)],
    flavor: &str,
    exceptions: &HashSet<(String, String)>,
) -> String {
    let mut s = String::from(GENERATED_HEADER);
    for (m, role_admin) in matched {
        s.push_str(&emit_one(m, *role_admin, flavor, exceptions));
        s.push('\n');
    }
    s
}

fn emit_one(
    m: &Method,
    role_admin: bool,
    flavor: &str,
    exceptions: &HashSet<(String, String)>,
) -> String {
    let verb = &m.ident; // unique, deterministic; prettified later.
    let struct_ident = format!("SurfaceArgs_{verb}");
    let mut fields = String::from("    pub endpoint: String,\n");
    let mut call_args = String::new();
    for p in &m.params {
        fields.push_str(&p.field_decl);
        fields.push('\n');
        call_args.push_str(&p.call_expr);
        call_args.push_str(", ");
    }
    let ret = m.ret.as_deref().unwrap_or("()");

    // Writes are data mutations. A write flagged `x-orca-user-callable` keeps
    // that classification but drops to `role = "read"` (callable by any read
    // identity); otherwise it stays `role = "admin"` (opt-in gated).
    let is_mutation = role_admin;
    let user_callable = is_mutation && exceptions.contains(&(m.http.clone(), m.path.clone()));
    let role = if user_callable {
        ", role = \"read\""
    } else if is_mutation {
        ", role = \"admin\""
    } else {
        ""
    };
    let data_mutation = if is_mutation {
        ", data_mutation = true"
    } else {
        ""
    };
    format!(
        "#[derive(Serialize, Deserialize, JsonSchema)]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {struct_ident} {{\n{fields}}}\n\n\
         /// Auto-generated from `{http} {path}`.\n\
         #[orca_tool(domain = \"{flavor}\", verb = \"{verb}\", cli = \"skip\"{role}{data_mutation})]\n\
         async fn surface_{verb}(args: {struct_ident}, _ctx: &ToolCtx) -> anyhow::Result<{ret}> {{\n    \
         let client = crate::tools::make_client(&args.endpoint).await?;\n    \
         let out = client.{ident}({call_args}).await.map_err(|e| anyhow::anyhow!(\"{flavor}.{verb}: {{e}}\"))?.into_inner();\n    \
         Ok(out)\n}}\n",
        http = m.http,
        path = m.path,
        ident = m.ident,
    )
}

// ── JsonSchema anchoring ────────────────────────────────────────────────────

/// All type idents defined under `pub mod types { ... }`.
fn collect_type_idents(file: &syn::File) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(items) = types_mod_items(file) {
        for it in items {
            match it {
                syn::Item::Struct(s) => {
                    set.insert(s.ident.to_string());
                }
                syn::Item::Enum(e) => {
                    set.insert(e.ident.to_string());
                }
                _ => {}
            }
        }
    }
    set
}

/// For each type ident, the local type idents it references — the adjacency for
/// the closure.
fn collect_type_field_refs(
    file: &syn::File,
    locals: &BTreeSet<String>,
) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(items) = types_mod_items(file) {
        for it in items {
            match it {
                syn::Item::Struct(s) => {
                    let mut refs = Vec::new();
                    for f in &s.fields {
                        collect_local_idents(&f.ty, locals, &mut refs);
                    }
                    map.insert(s.ident.to_string(), refs);
                }
                syn::Item::Enum(e) => {
                    let mut refs = Vec::new();
                    for v in &e.variants {
                        for f in &v.fields {
                            collect_local_idents(&f.ty, locals, &mut refs);
                        }
                    }
                    map.insert(e.ident.to_string(), refs);
                }
                _ => {}
            }
        }
    }
    map
}

fn close_over(seed: &str, refs: &HashMap<String, Vec<String>>, out: &mut BTreeSet<String>) {
    if !out.insert(seed.to_string()) {
        return;
    }
    if let Some(children) = refs.get(seed) {
        for c in children {
            close_over(c, refs, out);
        }
    }
}

/// Add `#[derive(JsonSchema)] #[schemars(crate=...)]` to every type in `needed`.
fn anchor_jsonschema(file: &mut syn::File, needed: &BTreeSet<String>) -> usize {
    let mut n = 0;
    if let Some(items) = types_mod_items_mut(file) {
        for it in items {
            let (ident, attrs) = match it {
                syn::Item::Struct(s) => (s.ident.to_string(), &mut s.attrs),
                syn::Item::Enum(e) => (e.ident.to_string(), &mut e.attrs),
                _ => continue,
            };
            if !needed.contains(&ident) {
                continue;
            }
            push_jsonschema_anchor(attrs);
            n += 1;
        }
    }
    n
}

// ── syn helpers (OpenAPI-closure specific) ──────────────────────────────────

fn types_mod_items(file: &syn::File) -> Option<&Vec<syn::Item>> {
    file.items.iter().find_map(|it| match it {
        syn::Item::Mod(m) if m.ident == "types" => m.content.as_ref().map(|(_, items)| items),
        _ => None,
    })
}

fn types_mod_items_mut(file: &mut syn::File) -> Option<&mut Vec<syn::Item>> {
    file.items.iter_mut().find_map(|it| match it {
        syn::Item::Mod(m) if m.ident == "types" => m.content.as_mut().map(|(_, items)| items),
        _ => None,
    })
}

/// If `ty` is `&[T]` or `Vec<T>`, return `T`.
fn slice_or_vec_inner(ty: &syn::Type) -> Option<&syn::Type> {
    match ty {
        syn::Type::Reference(r) => match &*r.elem {
            syn::Type::Slice(s) => Some(&s.elem),
            other => slice_or_vec_inner(other),
        },
        _ => first_generic(ty, "Vec"),
    }
}

/// Collect bare `types::Ident` leaf idents referenced anywhere in `ty`.
fn collect_types_idents_in_ty(ty: &syn::Type, out: &mut Vec<String>) {
    match ty {
        syn::Type::Path(p) => {
            if p.path.segments.first().is_some_and(|s| s.ident == "types")
                && let Some(last) = p.path.segments.last()
            {
                out.push(last.ident.to_string());
            }
            for seg in &p.path.segments {
                if let syn::PathArguments::AngleBracketed(a) = &seg.arguments {
                    for arg in &a.args {
                        if let syn::GenericArgument::Type(t) = arg {
                            collect_types_idents_in_ty(t, out);
                        }
                    }
                }
            }
        }
        syn::Type::Reference(r) => collect_types_idents_in_ty(&r.elem, out),
        syn::Type::Slice(s) => collect_types_idents_in_ty(&s.elem, out),
        syn::Type::Tuple(t) => t
            .elems
            .iter()
            .for_each(|e| collect_types_idents_in_ty(e, out)),
        _ => {}
    }
}

/// Collect local (defined-in-`types`) idents referenced in `ty` for adjacency.
fn collect_local_idents(ty: &syn::Type, locals: &BTreeSet<String>, out: &mut Vec<String>) {
    match ty {
        syn::Type::Path(p) => {
            if let Some(last) = p.path.segments.last() {
                let id = last.ident.to_string();
                if locals.contains(&id) {
                    out.push(id);
                }
                if let syn::PathArguments::AngleBracketed(a) = &last.arguments {
                    for arg in &a.args {
                        if let syn::GenericArgument::Type(t) = arg {
                            collect_local_idents(t, locals, out);
                        }
                    }
                }
            }
        }
        syn::Type::Reference(r) => collect_local_idents(&r.elem, locals, out),
        syn::Type::Slice(s) => collect_local_idents(&s.elem, locals, out),
        syn::Type::Tuple(t) => t
            .elems
            .iter()
            .for_each(|e| collect_local_idents(e, locals, out)),
        _ => {}
    }
}

/// Rewrite `types :: ...` occurrences (as `to_token_stream` renders them) to
/// `crate :: generated :: types :: ...` for use in emitted source.
fn rewrite_types_path(s: &str) -> String {
    s.replace("types ::", "crate :: generated :: types ::")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exceptions_pick_up_user_callable_ops_only() {
        let spec = json!({
            "paths": {
                "/vms": {
                    "get": { "operationId": "list_vms" },
                    "post": { "operationId": "create_vm", "x-orca-user-callable": true }
                },
                "/vms/{id}": {
                    "delete": { "operationId": "delete_vm" },
                    "put": { "operationId": "update_vm", "x-orca-user-callable": false }
                }
            }
        });
        let mut out = HashSet::new();
        exceptions_from_spec_value(&spec, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out.contains(&("POST".to_string(), "/vms".to_string())));
        assert!(!out.contains(&("PUT".to_string(), "/vms/{id}".to_string())));
    }

    #[test]
    fn exceptions_empty_when_no_paths() {
        let mut out = HashSet::new();
        exceptions_from_spec_value(&json!({"openapi": "3.0.0"}), &mut out);
        assert!(out.is_empty());
    }
}
