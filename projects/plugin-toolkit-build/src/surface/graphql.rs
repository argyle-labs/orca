//! GraphQL → orca tool-surface generator.
//!
//! Runs in a plugin `build.rs` *after* [`crate::graphql::generate`] has written
//! `<out_dir>/modules.rs`. Rather than hand-write one `#[orca_tool]` per GraphQL
//! operation, this walks the codegen'd query modules and emits one tool per
//! operation.
//!
//! It writes `<plugin>_surface.rs` — an `#[orca_tool]` wrapper per operation.
//! Operation kind comes from the parsed GraphQL document; a document that fails
//! to parse or classify aborts generation. Query operations surface as read
//! tools; mutations surface as `role = "admin"` control-plane tools. The args struct carries the operation's
//! `Variables` as typed fields plus the endpoint/override selection; the return
//! is the operation's typed `ResponseData`. It also anchors JsonSchema (and the
//! missing serde direction) onto every type in each surfaced operation module,
//! so the full request/response shape is runtime-introspectable via the tool's
//! arg/output schema.
//!
//! Every type in a `graphql_client` operation module belongs to either the
//! request tree (`Variables` + input objects, `Serialize`-only) or the response
//! tree (`ResponseData` + nested structs, `Deserialize`). So anchoring the whole
//! module is exactly the transitive closure — no graph walk needed.
//!
//! Per-file marker comments in the `.graphql` source adjust the emitted tool:
//! - `# @orca:admin` — force `role = "admin"` (for sensitive queries).
//! - `# @orca:no-tool` — bind the operation for codegen but emit no tool.
//! - `# @orca:data-mutation` — a mutation gains `data_mutation = true`, opening
//!   it to non-admin `can_mutate` callers.
//! - `# @orca:user-callable` — a mutation gains `data_mutation = true` and is
//!   emitted `role = "read"`.

#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use graphql_parser::query::{Definition, OperationDefinition};
use regex::Regex;

use crate::surface::common::{
    GENERATED_HEADER, derive_list, has_serde_crate_attr, push_jsonschema_anchor,
};

/// GraphQL scalar type aliases `graphql_client` emits inside every operation
/// module (`type Boolean = bool;` …). They are **not** `pub`, so a `Variables`
/// field typed as one must render to its primitive, not to a module path.
fn scalar_primitive(ident: &str) -> Option<&'static str> {
    Some(match ident {
        "Boolean" => "bool",
        "Float" => "f64",
        "Int" => "i64",
        "ID" | "String" => "String",
        _ => return None,
    })
}

const VERSION_SCALARS: &[&str] = &["BigInt", "DateTime", "JSON", "Port", "PrefixedID", "URL"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Markers {
    user_callable: bool,
    admin: bool,
    no_tool: bool,
    data_mutation: bool,
}

/// Rust primitives a `Variables` field can already be, rendered as-is.
const PRIMITIVES: &[&str] = &[
    "bool", "String", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "f32", "f64",
];

/// One surfaceable GraphQL operation.
struct Op {
    /// Snake-case module name (`add_plugin`) — also the tool verb.
    module: String,
    /// PascalCase marker struct implementing `GraphQLQuery` (`AddPlugin`).
    marker: String,
    /// `true` for `mutation` operations → `role = "admin"`.
    is_mutation: bool,
    /// `Variables` fields as `(name, rendered_type)`. Empty ⇒ unit `Variables`
    /// (no arguments).
    var_fields: Vec<(String, String)>,
}

/// Generate `<plugin>_surface.rs` from the codegen'd `<out_dir>/modules.rs`.
///
/// - `out_dir`: the codegen output dir (`OUT_DIR`). Reads (and rewrites, to
///   anchor JsonSchema) `<out_dir>/modules.rs`; writes `<plugin>_surface.rs`.
/// - `queries_dir`: the `.graphql` sources — scanned for `# @orca:*` markers.
/// - `plugin_name`: the tool domain + emitted-file prefix (`"unraid"`).
pub fn generate(out_dir: &Path, queries_dir: &Path, plugin_name: &str) -> Result<()> {
    let modules_path = out_dir.join("modules.rs");
    let src = std::fs::read_to_string(&modules_path)
        .with_context(|| format!("read {}", modules_path.display()))?;
    let mut file: syn::File = syn::parse_file(&src).context("parse generated modules.rs")?;

    let markers = operation_markers(queries_dir)?;

    // Newest committed version module (`v7_3_1`); ties break to the max ident,
    // matching the toolkit codegen's sort + `ApiVersion::newest`.
    let version =
        newest_version(&file).context("no `v<version>` module in generated modules.rs")?;
    let qualifier = format!("crate::generated::{version}");

    let gen_items = generated_items_mut(&mut file, &version)
        .context("no `generated` module inside the version module")?;

    let mut ops = Vec::new();
    let mut anchored = 0usize;
    let mut excluded = 0usize;
    for item in gen_items.iter_mut() {
        let syn::Item::Mod(m) = item else { continue };
        let Some((_, items)) = m.content.as_mut() else {
            continue;
        };
        let module = m.ident.to_string();
        let Some((marker, is_mutation)) =
            op_meta(items).with_context(|| format!("surface[{plugin_name}]: `{module}`"))?
        else {
            continue;
        };
        if markers.get(&marker).is_some_and(|m| m.no_tool) {
            excluded += 1;
            continue;
        }
        let mod_qualifier = format!("{qualifier}::{module}");
        let var_fields = match variables_fields(items, &qualifier, &mod_qualifier) {
            Some(f) => f,
            None => {
                println!(
                    "cargo:warning=surface[{plugin_name}]: skipped `{module}` — unrenderable Variables shape"
                );
                continue;
            }
        };
        anchored += anchor_module(items);
        ops.push(Op {
            module,
            marker,
            is_mutation,
            var_fields,
        });
    }

    std::fs::write(&modules_path, prettyplease::unparse(&file))
        .with_context(|| format!("rewrite {}", modules_path.display()))?;

    let exception_hits = ops
        .iter()
        .filter(|o| o.is_mutation && markers.get(&o.marker).is_some_and(|m| m.user_callable))
        .count();
    let surface = emit_surface(&ops, &qualifier, plugin_name, &markers);
    let surface_path = out_dir.join(format!("{plugin_name}_surface.rs"));
    std::fs::write(&surface_path, surface)
        .with_context(|| format!("write {}", surface_path.display()))?;

    println!(
        "cargo:warning=surface[{plugin_name}]: {} tool(s) emitted from {version}, \
         JsonSchema anchored on {anchored} type(s), {exception_hits} user-callable exception(s), \
         {excluded} excluded",
        ops.len()
    );
    Ok(())
}

fn operation_markers(queries_dir: &Path) -> Result<HashMap<String, Markers>> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(queries_dir) else {
        return Ok(out);
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("graphql") {
            continue;
        }
        println!("cargo:rerun-if-changed={}", p.display());
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let m = file_markers(&text);
        if m == Markers::default() {
            continue;
        }
        for name in operation_names(&text) {
            out.insert(name, m);
        }
    }
    Ok(out)
}

fn file_markers(text: &str) -> Markers {
    let mut m = Markers::default();
    for l in text.lines() {
        let Some(rest) = l.trim().strip_prefix('#') else {
            continue;
        };
        let t = rest.trim_start_matches('#').trim().to_ascii_lowercase();
        let Some(name) = t.strip_prefix("@orca:") else {
            continue;
        };
        let name = name.split_whitespace().next().unwrap_or("");
        match name {
            "user-callable" => m.user_callable = true,
            "admin" => m.admin = true,
            "no-tool" => m.no_tool = true,
            "data-mutation" => m.data_mutation = true,
            _ => {}
        }
    }
    m
}

/// The operation names declared in a `.graphql` document (`query X`,
/// `mutation X`, `subscription X`).
fn operation_names(text: &str) -> Vec<String> {
    let re =
        Regex::new(r"(?m)\b(?:query|mutation|subscription)\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    re.captures_iter(text).map(|c| c[1].to_string()).collect()
}

/// The lexicographically-greatest `v*` module ident (newest version).
fn newest_version(file: &syn::File) -> Option<String> {
    file.items
        .iter()
        .filter_map(|it| match it {
            syn::Item::Mod(m) => {
                let id = m.ident.to_string();
                (id.starts_with('v') && id[1..].starts_with(|c: char| c.is_ascii_digit()))
                    .then_some(id)
            }
            _ => None,
        })
        .max()
}

/// Mutable access to the items inside `<version>::generated { ... }`.
fn generated_items_mut<'a>(
    file: &'a mut syn::File,
    version: &str,
) -> Option<&'a mut Vec<syn::Item>> {
    let ver_items = file.items.iter_mut().find_map(|it| match it {
        syn::Item::Mod(m) if m.ident == version => m.content.as_mut().map(|(_, i)| i),
        _ => None,
    })?;
    ver_items.iter_mut().find_map(|it| match it {
        syn::Item::Mod(m) if m.ident == "generated" => m.content.as_mut().map(|(_, i)| i),
        _ => None,
    })
}

/// Read an operation module's `OPERATION_NAME` (marker struct) and classify it
/// as query vs mutation from the parsed `QUERY` document. `Ok(None)` when the
/// module isn't an operation module (no `OPERATION_NAME` const); `Err` when the
/// kind can't be determined unambiguously.
fn op_meta(items: &[syn::Item]) -> Result<Option<(String, bool)>> {
    let Some(marker) = const_str_value(items, "OPERATION_NAME") else {
        return Ok(None);
    };
    let query = const_str_value(items, "QUERY")
        .with_context(|| format!("operation `{marker}` has no QUERY document"))?;
    let is_mutation = classify(&query, &marker)?;
    Ok(Some((marker, is_mutation)))
}

fn classify(doc: &str, name: &str) -> Result<bool> {
    let parsed = graphql_parser::parse_query::<&str>(doc)
        .map_err(|e| anyhow::anyhow!("parse operation `{name}`: {e}"))?;
    let ops: Vec<&OperationDefinition<'_, &str>> = parsed
        .definitions
        .iter()
        .filter_map(|d| match d {
            Definition::Operation(o) => Some(o),
            Definition::Fragment(_) => None,
        })
        .collect();
    let named: Vec<_> = ops.iter().filter(|o| op_name(o) == Some(name)).collect();
    let op = match (named.as_slice(), ops.as_slice()) {
        ([one], _) => **one,
        ([], [only]) => *only,
        _ => bail!("operation `{name}` is missing or ambiguous in its document"),
    };
    match op {
        OperationDefinition::SelectionSet(_) | OperationDefinition::Query(_) => Ok(false),
        OperationDefinition::Mutation(_) => Ok(true),
        OperationDefinition::Subscription(_) => {
            bail!("operation `{name}` is a subscription; not supported")
        }
    }
}

fn op_name<'a>(o: &OperationDefinition<'a, &'a str>) -> Option<&'a str> {
    match o {
        OperationDefinition::SelectionSet(_) => None,
        OperationDefinition::Query(q) => q.name,
        OperationDefinition::Mutation(m) => m.name,
        OperationDefinition::Subscription(s) => s.name,
    }
}

/// Value of a `pub const <name>: &str = "...";` item.
fn const_str_value(items: &[syn::Item], name: &str) -> Option<String> {
    items.iter().find_map(|it| match it {
        syn::Item::Const(c) if c.ident == name => match &*c.expr {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

/// Collect the `Variables` struct's fields as `(name, rendered_type)`. Returns
/// `Some(vec![])` for a unit `Variables` (no args), and `None` if any field type
/// can't be confidently rendered (⇒ skip surfacing that op rather than emit
/// broken code).
fn variables_fields(
    items: &[syn::Item],
    version_qualifier: &str,
    mod_qualifier: &str,
) -> Option<Vec<(String, String)>> {
    let vars = items.iter().find_map(|it| match it {
        syn::Item::Struct(s) if s.ident == "Variables" => Some(s),
        _ => None,
    })?;
    match &vars.fields {
        syn::Fields::Unit => Some(vec![]),
        syn::Fields::Named(named) => {
            let mut out = Vec::new();
            for f in &named.named {
                let name = f.ident.as_ref()?.to_string();
                let rendered = render_type(&f.ty, version_qualifier, mod_qualifier)?;
                out.push((name, rendered));
            }
            Some(out)
        }
        syn::Fields::Unnamed(_) => None,
    }
}

/// Render a `Variables` field type into a path usable from the emitted surface
/// module. Rust primitives pass through; GraphQL scalar aliases map to their
/// primitive; a custom scalar qualifies to the version module; a bare local
/// ident (an input object) qualifies to its module path. Anything else
/// (generics, references) ⇒ `None`.
fn render_type(ty: &syn::Type, version_qualifier: &str, mod_qualifier: &str) -> Option<String> {
    let syn::Type::Path(p) = ty else { return None };
    if p.qself.is_some() || p.path.segments.len() != 1 {
        return None;
    }
    let seg = &p.path.segments[0];
    if !matches!(seg.arguments, syn::PathArguments::None) {
        return None;
    }
    let ident = seg.ident.to_string();
    if PRIMITIVES.contains(&ident.as_str()) {
        return Some(ident);
    }
    if let Some(prim) = scalar_primitive(&ident) {
        return Some(prim.to_string());
    }
    if VERSION_SCALARS.contains(&ident.as_str()) {
        return Some(format!("{version_qualifier}::{ident}"));
    }
    // A locally-defined input object — reachable at the module path once we
    // anchor Deserialize/JsonSchema on it.
    Some(format!("{mod_qualifier}::{ident}"))
}

/// Make every struct/enum in an operation module a full tool type: derive
/// `Serialize + Deserialize + JsonSchema`. `graphql_client` emits only one serde
/// direction per type (variables side = `Serialize`, response side =
/// `Deserialize`), but `#[orca_tool]` needs args to *deserialize* and outputs to
/// *serialize*, and both need `JsonSchema`. `#[serde(crate = ...)]` is already
/// present on every derive-based generated type, so the added impls resolve
/// serde correctly. Returns the count touched.
fn anchor_module(items: &mut [syn::Item]) -> usize {
    let mut n = 0;
    for it in items.iter_mut() {
        let attrs = match it {
            syn::Item::Struct(s) => &mut s.attrs,
            syn::Item::Enum(e) => &mut e.attrs,
            _ => continue,
        };
        let derives = derive_list(attrs);
        let has = |name: &str| derives.iter().any(|d| d == name);
        if !has("JsonSchema") {
            push_jsonschema_anchor(attrs);
        }
        // Only *derive-based* serde types (which carry `#[serde(crate = ...)]`)
        // may gain a missing serde derive — it honors that crate anchor.
        // GraphQL enums instead ship hand-written `Serialize` + `Deserialize`
        // impls (both directions) and carry no `#[serde(crate)]`; adding a serde
        // derive there emits a bare `serde` reference (E0463) and collides with
        // the manual impl. They already round-trip, so JsonSchema is all they
        // need.
        if has_serde_crate_attr(attrs) {
            if !has("Serialize") {
                attrs.push(syn::parse_quote!(
                    #[derive(::plugin_toolkit::serde::Serialize)]
                ));
            }
            if !has("Deserialize") {
                attrs.push(syn::parse_quote!(
                    #[derive(::plugin_toolkit::serde::Deserialize)]
                ));
            }
        }
        n += 1;
    }
    n
}

fn emit_surface(
    ops: &[Op],
    qualifier: &str,
    plugin_name: &str,
    markers: &HashMap<String, Markers>,
) -> String {
    let mut s = String::from(GENERATED_HEADER);
    for op in ops {
        let m = markers.get(&op.marker).copied().unwrap_or_default();
        s.push_str(&emit_one(op, qualifier, plugin_name, m));
        s.push('\n');
    }
    s
}

fn emit_one(op: &Op, qualifier: &str, plugin_name: &str, markers: Markers) -> String {
    let Op {
        module,
        marker,
        is_mutation,
        var_fields,
    } = op;
    let args_ident = format!("SurfaceArgs_{module}");
    let mod_path = format!("{qualifier}::{module}");
    let marker_path = format!("{qualifier}::{marker}");

    let mut fields = format!(
        "    /// Registered endpoint name (from `{plugin_name}.list`); used when no explicit `from`.\n    \
         pub endpoint: Option<String>,\n    \
         /// Explicit base-URL override (wins over `endpoint`); pair with `api_key`.\n    \
         pub from: Option<String>,\n    \
         /// Explicit API key for the `from` override.\n    \
         pub api_key: Option<String>,\n    \
         /// Accept self-signed TLS for the `from` override.\n    \
         pub insecure: Option<bool>,\n"
    );
    for (name, ty) in var_fields {
        fields.push_str(&format!("    pub {name}: {ty},\n"));
    }

    let vars_expr = if var_fields.is_empty() {
        format!("{mod_path}::Variables")
    } else {
        let inits = var_fields
            .iter()
            .map(|(name, _)| format!("{name}: args.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{mod_path}::Variables {{ {inits} }}")
    };

    let user_callable_op = *is_mutation && markers.user_callable;
    let role = if markers.admin || (*is_mutation && !user_callable_op) {
        ", role = \"admin\""
    } else if user_callable_op {
        ", role = \"read\""
    } else {
        ""
    };
    let data_mutation = if user_callable_op || (*is_mutation && markers.data_mutation) {
        ", data_mutation = true"
    } else {
        ""
    };
    let kind = if *is_mutation { "mutation" } else { "query" };

    format!(
        "#[derive(Serialize, Deserialize, JsonSchema)]\n\
         #[serde(crate = \"::plugin_toolkit::serde\")]\n\
         #[schemars(crate = \"::plugin_toolkit::schemars\")]\n\
         #[allow(non_camel_case_types)]\n\
         pub struct {args_ident} {{\n{fields}}}\n\n\
         /// Auto-generated from the `{marker}` GraphQL {kind}.\n\
         #[orca_tool(domain = \"{plugin_name}\", verb = \"{module}\", cli = \"skip\"{role}{data_mutation})]\n\
         async fn surface_{module}(args: {args_ident}, _ctx: &ToolCtx) -> Result<{mod_path}::ResponseData> {{\n    \
         let client = crate::tools::surface_client(args.endpoint, args.from, args.api_key, args.insecure).await?;\n    \
         let vars = {vars_expr};\n    \
         client\n        \
         .query::<{marker_path}>(vars)\n        \
         .await\n        \
         .map_err(|e| anyhow!(\"{plugin_name}.{module}: {{e}}\"))\n}}\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_detected_various_spellings() {
        let uc = |t: &str| file_markers(t).user_callable;
        assert!(uc("# @orca:user-callable\nmutation X { y }"));
        assert!(uc("#   @Orca:User-Callable  (safe idempotent)\n"));
        assert!(uc("mutation X { y }\n#@orca:user-callable"));
        assert!(!uc("mutation X { y }"));
        assert!(!uc("# just a normal comment"));
    }

    #[test]
    fn file_markers_reads_each_marker() {
        let m =
            file_markers("# @orca:admin\n# @orca:no-tool\n## @orca:data-mutation\nquery X { y }");
        assert_eq!(
            m,
            Markers {
                user_callable: false,
                admin: true,
                no_tool: true,
                data_mutation: true,
            }
        );
        assert_eq!(file_markers("# @orca:administrator"), Markers::default());
    }

    #[test]
    fn operation_names_extracted() {
        let names =
            operation_names("# @orca:user-callable\nmutation AddPlugin($i: In!) { addPlugin }");
        assert_eq!(names, vec!["AddPlugin".to_string()]);
        let q = operation_names("query ArrayStatus {\n array { id }\n}");
        assert_eq!(q, vec!["ArrayStatus".to_string()]);
    }

    #[test]
    fn user_callable_set_keys_on_operation_name() {
        // A file with the marker contributes its operation name to the set;
        // `emit_one` matches that against `Op.marker` (OPERATION_NAME).
        let text = "# @orca:user-callable\nmutation RestartArray { restartArray }";
        assert!(file_markers(text).user_callable);
        assert_eq!(operation_names(text), vec!["RestartArray".to_string()]);
    }

    #[test]
    fn operation_names_multiple_kinds() {
        let names = operation_names(
            "query A { a }\nmutation B { b }\nsubscription C { c }\nfragment F on T { x }",
        );
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn scalar_primitive_maps_known_rejects_unknown() {
        assert_eq!(scalar_primitive("Boolean"), Some("bool"));
        assert_eq!(scalar_primitive("Float"), Some("f64"));
        assert_eq!(scalar_primitive("Int"), Some("i64"));
        assert_eq!(scalar_primitive("ID"), Some("String"));
        assert_eq!(scalar_primitive("String"), Some("String"));
        assert_eq!(scalar_primitive("MyInput"), None);
    }

    fn ty(s: &str) -> syn::Type {
        syn::parse_str(s).unwrap()
    }

    #[test]
    fn render_type_primitive_scalar_and_input_object() {
        // Rust primitive passes through.
        assert_eq!(render_type(&ty("i64"), "v", "q::m").as_deref(), Some("i64"));
        assert_eq!(
            render_type(&ty("bool"), "v", "q::m").as_deref(),
            Some("bool")
        );
        // GraphQL scalar alias → primitive.
        assert_eq!(render_type(&ty("Int"), "v", "q::m").as_deref(), Some("i64"));
        assert_eq!(
            render_type(&ty("ID"), "v", "q::m").as_deref(),
            Some("String")
        );
        // Local input object → qualified module path.
        assert_eq!(
            render_type(&ty("AddPluginInput"), "v", "q::m").as_deref(),
            Some("q::m::AddPluginInput")
        );
        assert_eq!(
            render_type(&ty("PrefixedID"), "v", "q::m").as_deref(),
            Some("v::PrefixedID")
        );
        assert_eq!(
            render_type(&ty("Port"), "v", "q::m").as_deref(),
            Some("v::Port")
        );
        assert_eq!(
            render_type(&ty("JSON"), "v", "q::m").as_deref(),
            Some("v::JSON")
        );
        assert_eq!(
            render_type(&ty("URL"), "v", "q::m").as_deref(),
            Some("v::URL")
        );
    }

    #[test]
    fn render_type_rejects_generics_refs_and_multiseg() {
        assert_eq!(render_type(&ty("Option<String>"), "v", "q::m"), None);
        assert_eq!(render_type(&ty("Vec<i64>"), "v", "q::m"), None);
        assert_eq!(render_type(&ty("&str"), "v", "q::m"), None);
        assert_eq!(render_type(&ty("std::string::String"), "v", "q::m"), None);
    }

    fn items(src: &str) -> Vec<syn::Item> {
        syn::parse_str::<syn::File>(src).unwrap().items
    }

    #[test]
    fn const_str_value_reads_str_const_only() {
        let its = items("pub const OPERATION_NAME: &str = \"AddPlugin\";\npub const N: i32 = 3;");
        assert_eq!(
            const_str_value(&its, "OPERATION_NAME").as_deref(),
            Some("AddPlugin")
        );
        assert_eq!(const_str_value(&its, "N"), None);
        assert_eq!(const_str_value(&its, "MISSING"), None);
    }

    fn meta(name: &str, query: &str) -> Result<Option<(String, bool)>> {
        op_meta(&items(&format!(
            "pub const OPERATION_NAME: &str = {name:?};\npub const QUERY: &str = {query:?};"
        )))
    }

    #[test]
    fn op_meta_classifies_query_vs_mutation() {
        assert_eq!(
            meta("AddPlugin", "  mutation AddPlugin { x }").unwrap(),
            Some(("AddPlugin".into(), true))
        );
        assert_eq!(
            meta("Status", "query Status { x }").unwrap(),
            Some(("Status".into(), false))
        );
        assert_eq!(meta("Anon", "{ x }").unwrap(), Some(("Anon".into(), false)));
        assert!(op_meta(&items("pub const OPERATION_NAME: &str = \"Bare\";")).is_err());
        assert_eq!(op_meta(&items("pub struct Foo;")).unwrap(), None);
    }

    #[test]
    fn op_meta_commented_mutation_is_mutation() {
        let got = meta(
            "InstallPlugin",
            "# installs a plugin\n# query-looking comment\nmutation InstallPlugin($u: String!) { installPlugin(url: $u) }",
        )
        .unwrap();
        assert_eq!(got, Some(("InstallPlugin".into(), true)));
        let o = op("install_plugin", "InstallPlugin", true, vec![]);
        let out = emit_one(&o, "crate::generated::v1", "unraid", Markers::default());
        assert!(out.contains("role = \"admin\""));
        assert!(!out.contains("data_mutation"));
    }

    #[test]
    fn op_meta_picks_named_operation_among_many() {
        let doc = "query A { a }\nmutation B { b }\nfragment F on T { x }";
        assert_eq!(meta("B", doc).unwrap(), Some(("B".into(), true)));
        assert_eq!(meta("A", doc).unwrap(), Some(("A".into(), false)));
        assert!(meta("C", doc).is_err());
    }

    #[test]
    fn op_meta_parse_failure_and_subscription_error() {
        assert!(meta("X", "mutation X { ").is_err());
        assert!(meta("X", "not graphql at all").is_err());
        assert!(meta("S", "subscription S { s }").is_err());
    }

    #[test]
    fn variables_fields_unit_named_and_unrenderable() {
        // Unit Variables ⇒ Some(empty).
        assert_eq!(
            variables_fields(&items("pub struct Variables;"), "v", "q::m"),
            Some(vec![])
        );
        // Named fields render.
        let named =
            items("pub struct Variables { pub id: String, pub count: Int, pub input: MyInput }");
        assert_eq!(
            variables_fields(&named, "v", "q::m"),
            Some(vec![
                ("id".into(), "String".into()),
                ("count".into(), "i64".into()),
                ("input".into(), "q::m::MyInput".into()),
            ])
        );
        // A field we cannot render ⇒ None (skip whole op).
        assert_eq!(
            variables_fields(
                &items("pub struct Variables { pub x: Option<String> }"),
                "v",
                "q::m"
            ),
            None
        );
        // Tuple struct Variables ⇒ None.
        assert_eq!(
            variables_fields(&items("pub struct Variables(String);"), "v", "q::m"),
            None
        );
        // No Variables struct ⇒ None.
        assert_eq!(
            variables_fields(&items("pub struct Other;"), "v", "q::m"),
            None
        );
    }

    #[test]
    fn newest_version_picks_lexicographic_max() {
        let f: syn::File =
            syn::parse_str("mod v1_0_0 {}\nmod v7_3_1 {}\nmod v2_0_0 {}\nmod helpers {}").unwrap();
        assert_eq!(newest_version(&f).as_deref(), Some("v7_3_1"));

        let none: syn::File = syn::parse_str("mod helpers {}\nmod vx {}").unwrap();
        assert_eq!(newest_version(&none), None);
    }

    #[test]
    fn generated_items_mut_finds_nested_generated() {
        let mut f: syn::File =
            syn::parse_str("mod v1_0_0 { mod generated { struct A; } }").unwrap();
        let got = generated_items_mut(&mut f, "v1_0_0");
        assert!(got.is_some());
        assert_eq!(got.unwrap().len(), 1);

        // Version module without a `generated` child ⇒ None.
        let mut f2: syn::File = syn::parse_str("mod v1_0_0 { mod other { struct A; } }").unwrap();
        assert!(generated_items_mut(&mut f2, "v1_0_0").is_none());
    }

    #[test]
    fn anchor_module_adds_missing_derives() {
        // A derive-based serde type (carries `#[serde(crate = ...)]`) missing
        // JsonSchema + one serde direction gains both.
        let mut its = items(
            "#[derive(::plugin_toolkit::serde::Serialize)]\n#[serde(crate = \"x\")]\npub struct V { pub a: String }",
        );
        let n = anchor_module(&mut its);
        assert_eq!(n, 1);
        let attrs = match &its[0] {
            syn::Item::Struct(s) => &s.attrs,
            _ => unreachable!(),
        };
        let derives = derive_list(attrs);
        assert!(derives.iter().any(|d| d == "JsonSchema"));
        assert!(derives.iter().any(|d| d == "Deserialize"));
        assert!(derives.iter().any(|d| d == "Serialize"));

        // A hand-serde type (no `#[serde(crate)]`) gains only JsonSchema.
        let mut hand = items("pub enum E { A, B }");
        anchor_module(&mut hand);
        let hattrs = match &hand[0] {
            syn::Item::Enum(e) => &e.attrs,
            _ => unreachable!(),
        };
        let hderives = derive_list(hattrs);
        assert!(hderives.iter().any(|d| d == "JsonSchema"));
        assert!(!hderives.iter().any(|d| d == "Serialize"));
    }

    fn op(module: &str, marker: &str, is_mutation: bool, vars: Vec<(&str, &str)>) -> Op {
        Op {
            module: module.into(),
            marker: marker.into(),
            is_mutation,
            var_fields: vars
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    #[test]
    fn emit_one_query_no_role_no_mutation() {
        let o = op("array_status", "ArrayStatus", false, vec![]);
        let out = emit_one(&o, "crate::generated::v1", "unraid", Markers::default());
        assert!(out.contains("verb = \"array_status\""));
        assert!(!out.contains("data_mutation = true"));
        assert!(!out.contains("role ="));
        // Unit Variables ⇒ bare `Variables` expr.
        assert!(out.contains("crate::generated::v1::array_status::Variables;"));
        assert!(out.contains("Auto-generated from the `ArrayStatus` GraphQL query"));
    }

    #[test]
    fn emit_one_mutation_admin_by_default() {
        let o = op(
            "restart_array",
            "RestartArray",
            true,
            vec![("force", "bool")],
        );
        let out = emit_one(&o, "crate::generated::v1", "unraid", Markers::default());
        assert!(!out.contains("data_mutation"));
        assert!(out.contains("role = \"admin\""));
        assert!(out.contains("pub force: bool,"));
        // Named vars ⇒ struct init from args.
        assert!(out.contains("Variables { force: args.force }"));
    }

    #[test]
    fn emit_one_mutation_user_callable_drops_to_read() {
        let o = op("add_plugin", "AddPlugin", true, vec![]);
        let m = Markers {
            user_callable: true,
            ..Markers::default()
        };
        let out = emit_one(&o, "crate::generated::v1", "unraid", m);
        assert!(out.contains("data_mutation = true"));
        assert!(out.contains("role = \"read\""));
        assert!(!out.contains("role = \"admin\""));
    }

    #[test]
    fn emit_one_mutation_data_mutation_marker_opts_in() {
        let o = op("add_share", "AddShare", true, vec![]);
        let m = Markers {
            data_mutation: true,
            ..Markers::default()
        };
        let out = emit_one(&o, "crate::generated::v1", "unraid", m);
        assert!(out.contains("data_mutation = true"));
        assert!(out.contains("role = \"admin\""));
    }

    #[test]
    fn emit_one_admin_marker_forces_admin() {
        let admin = Markers {
            admin: true,
            ..Markers::default()
        };
        let q = op("api_keys", "ApiKeys", false, vec![]);
        let out = emit_one(&q, "crate::generated::v1", "unraid", admin);
        assert!(out.contains("role = \"admin\""));
        assert!(!out.contains("data_mutation"));

        let both = Markers {
            admin: true,
            user_callable: true,
            ..Markers::default()
        };
        let m = op("add_plugin", "AddPlugin", true, vec![]);
        let out = emit_one(&m, "crate::generated::v1", "unraid", both);
        assert!(out.contains("role = \"admin\""));
    }

    #[test]
    fn emit_surface_prepends_header_and_all_ops() {
        let ops = vec![op("a", "A", false, vec![]), op("b", "B", true, vec![])];
        let out = emit_surface(&ops, "crate::generated::v1", "demo", &HashMap::new());
        assert!(out.starts_with(GENERATED_HEADER));
        assert!(out.contains("verb = \"a\""));
        assert!(out.contains("verb = \"b\""));
    }

    #[test]
    fn operation_markers_reads_marked_files() {
        let dir = std::env::temp_dir().join(format!(
            "gql_uc_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("marked.graphql"),
            "# @orca:user-callable\nmutation AddPlugin { addPlugin }",
        )
        .unwrap();
        std::fs::write(
            dir.join("plain.graphql"),
            "mutation RemovePlugin { removePlugin }",
        )
        .unwrap();
        std::fs::write(
            dir.join("hidden.graphql"),
            "# @orca:no-tool\nquery Secrets { secrets }",
        )
        .unwrap();
        std::fs::write(dir.join("ignore.txt"), "# @orca:admin\nmutation Nope { x }").unwrap();

        let set = operation_markers(&dir).unwrap();
        assert!(set["AddPlugin"].user_callable);
        assert!(set["Secrets"].no_tool);
        assert!(!set.contains_key("RemovePlugin"));
        assert!(!set.contains_key("Nope"));

        assert!(operation_markers(&dir.join("nope")).unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }
}
