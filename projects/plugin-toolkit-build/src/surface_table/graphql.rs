#![allow(clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ::openapi::surface::{
    ApiOp, ApiSurface, Auth, Base, EndpointBinding, FORMAT, Param, ParamIn, ResponseKind,
    ResponseRules, SurfaceKind,
};
use anyhow::{Context, Result, bail};
use graphql_parser::query::{Definition, Document, OperationDefinition, Type, VariableDefinition};
use serde_json::{Map, Value, json};

use crate::surface::common::tool_policy;
use crate::surface::graphql::operation_markers;

const RUST_KEYWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub api_root: String,
    pub graphql_path: Option<String>,
    pub auth: Option<Auth>,
    pub response: ResponseRules,
    pub endpoint: EndpointBinding,
    pub renames: BTreeMap<String, String>,
}

pub fn generate(
    schemas_dir: &Path,
    queries_dir: &Path,
    out_dir: &Path,
    surface: &str,
    opts: &Options,
) -> Result<PathBuf> {
    println!("cargo:rerun-if-changed={}", schemas_dir.display());
    println!("cargo:rerun-if-changed={}", queries_dir.display());
    let (_, schema_path) = crate::graphql::schema_versions(schemas_dir)
        .pop()
        .with_context(|| format!("no schemas in {}", schemas_dir.display()))?;
    let schema = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("read {}", schema_path.display()))?;
    let markers = operation_markers(queries_dir)?;
    let mut documents = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(queries_dir)
        .with_context(|| format!("read {}", queries_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("graphql"))
        .collect();
    entries.sort();
    for p in entries {
        let text = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        documents.push(text);
    }
    let no_tool: BTreeSet<String> = markers
        .iter()
        .filter(|(_, m)| m.no_tool)
        .map(|(n, _)| n.clone())
        .collect();
    let table = build(&schema, &documents, surface, opts, |name, is_mutation| {
        let mk = markers.get(name).copied().unwrap_or_default();
        let (role, data_mutation) = tool_policy(is_mutation, mk);
        (role, data_mutation, no_tool.contains(name))
    })?;
    super::write(&table, out_dir)
}

pub fn build(
    schema: &str,
    documents: &[String],
    surface: &str,
    opts: &Options,
    policy: impl Fn(&str, bool) -> (&'static str, bool, bool),
) -> Result<ApiSurface> {
    let schema: Value = serde_json::from_str(schema).context("parse introspection schema")?;
    let types = schema_types(&schema)?;
    let mut pending: Vec<String> = Vec::new();
    let mut ops = Vec::new();
    let mut renamed = BTreeSet::new();
    for text in documents {
        let doc = graphql_parser::parse_query::<&str>(text)
            .map_err(|e| anyhow::anyhow!("parse graphql document: {e}"))?;
        let fragments: Vec<Definition<'_, &str>> = doc
            .definitions
            .iter()
            .filter(|d| matches!(d, Definition::Fragment(_)))
            .cloned()
            .collect();
        for def in &doc.definitions {
            let Definition::Operation(op) = def else {
                continue;
            };
            let (name, is_mutation, vars) = match op {
                OperationDefinition::Query(q) => (q.name, false, &q.variable_definitions),
                OperationDefinition::Mutation(m) => (m.name, true, &m.variable_definitions),
                OperationDefinition::Subscription(_) => bail!("subscriptions are not supported"),
                OperationDefinition::SelectionSet(_) => bail!("anonymous operation"),
            };
            let name = name.context("anonymous operation")?;
            let tail = heck04::ToSnakeCase::to_snake_case(name);
            let mut params = Vec::with_capacity(vars.len());
            for var in vars {
                params.push(variable_param(var, &types, &mut pending)?);
            }
            let mut single = vec![def.clone()];
            single.extend(fragments.iter().cloned());
            let (role, data_mutation, internal) = policy(name, is_mutation);
            let tool = match opts.renames.get(&tail) {
                Some(to) => {
                    renamed.insert(tail.clone());
                    to.clone()
                }
                None => tail.clone(),
            };
            ops.push(ApiOp {
                policy: super::derived_policy(&tool, is_mutation, role, data_mutation),
                tool,
                method: "POST".to_string(),
                path: Document {
                    definitions: single,
                }
                .to_string(),
                params,
                body: None,
                output: None,
                response: ResponseKind::Json,
                internal,
            });
        }
    }
    let mut sorted = BTreeMap::new();
    while let Some(name) = pending.pop() {
        if sorted.contains_key(&name) {
            continue;
        }
        let def = named_def(&name, &types, &mut pending)?;
        sorted.insert(name, def);
    }
    let defs: Map<String, Value> = sorted.into_iter().collect();

    if let Some(from) = opts.renames.keys().find(|k| !renamed.contains(*k)) {
        bail!("rename `{from}` matches no operation");
    }
    let mut names = BTreeSet::new();
    for op in &mut ops {
        op.tool = format!("{surface}.{}", op.tool);
        if !names.insert(op.tool.clone()) {
            bail!("duplicate tool `{}`", op.tool);
        }
    }
    ops.sort_by(|a, b| a.tool.cmp(&b.tool));
    Ok(ApiSurface {
        format: FORMAT.to_string(),
        surface: surface.to_string(),
        kind: SurfaceKind::Graphql,
        base: Base {
            api_root: opts.api_root.clone(),
            graphql_path: opts.graphql_path.clone(),
        },
        auth: opts.auth.clone(),
        response: opts.response.clone(),
        endpoint: opts.endpoint.clone(),
        defs,
        ops,
    })
}

fn rust_field_name(wire: &str) -> String {
    let snake = heck04::ToSnakeCase::to_snake_case(wire);
    if RUST_KEYWORDS.contains(&snake.as_str()) {
        format!("{snake}_")
    } else {
        snake
    }
}

fn schema_types(schema: &Value) -> Result<BTreeMap<String, Value>> {
    let root = schema.get("data").unwrap_or(schema);
    let list = root
        .pointer("/__schema/types")
        .and_then(Value::as_array)
        .context("introspection has no __schema.types")?;
    Ok(list
        .iter()
        .filter_map(|t| Some((t.get("name")?.as_str()?.to_string(), t.clone())))
        .collect())
}

fn variable_param<'a>(
    var: &VariableDefinition<'a, &'a str>,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Param> {
    let name = rust_field_name(var.name);
    Ok(Param {
        wire: (name != var.name).then(|| var.name.to_string()),
        name,
        location: ParamIn::Body,
        required: matches!(var.var_type, Type::NonNullType(_)),
        schema: ast_schema(&var.var_type, types, pending)?,
    })
}

fn ast_schema<'a>(
    ty: &Type<'a, &'a str>,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    match ty {
        Type::NonNullType(inner) => ast_schema_nonnull(inner, types, pending),
        other => Ok(nullable(ast_schema_nonnull(other, types, pending)?)),
    }
}

fn ast_schema_nonnull<'a>(
    ty: &Type<'a, &'a str>,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    match ty {
        Type::NonNullType(inner) => ast_schema_nonnull(inner, types, pending),
        Type::ListType(inner) => {
            Ok(json!({"type": "array", "items": ast_schema(inner, types, pending)?}))
        }
        Type::NamedType(name) => named_schema(name, types, pending),
    }
}

fn introspection_schema(
    ty: &Value,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    match ty.get("kind").and_then(Value::as_str) {
        Some("NON_NULL") => introspection_schema_nonnull(&ty["ofType"], types, pending),
        _ => Ok(nullable(introspection_schema_nonnull(ty, types, pending)?)),
    }
}

fn introspection_schema_nonnull(
    ty: &Value,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    match ty.get("kind").and_then(Value::as_str) {
        Some("NON_NULL") => introspection_schema_nonnull(&ty["ofType"], types, pending),
        Some("LIST") => Ok(
            json!({"type": "array", "items": introspection_schema(&ty["ofType"], types, pending)?}),
        ),
        _ => {
            let name = ty
                .get("name")
                .and_then(Value::as_str)
                .context("type reference without a name")?;
            named_schema(name, types, pending)
        }
    }
}

fn named_schema(
    name: &str,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    Ok(match name {
        "Int" => json!({"type": "integer"}),
        "Float" => json!({"type": "number"}),
        "String" | "ID" => json!({"type": "string"}),
        "Boolean" => json!({"type": "boolean"}),
        _ => match types
            .get(name)
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
        {
            Some("SCALAR") => json!({}),
            Some("ENUM" | "INPUT_OBJECT") => {
                pending.push(name.to_string());
                json!({"$ref": format!("#/$defs/{name}")})
            }
            other => bail!("input type `{name}` has unsupported kind {other:?}"),
        },
    })
}

fn nullable(schema: Value) -> Value {
    match schema {
        Value::Object(mut m) if m.contains_key("type") => {
            m.insert("nullable".into(), Value::Bool(true));
            Value::Object(m)
        }
        Value::Object(m) if m.is_empty() => Value::Object(m),
        other => json!({"anyOf": [other, {"type": "null"}]}),
    }
}

fn named_def(
    name: &str,
    types: &BTreeMap<String, Value>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    let ty = types
        .get(name)
        .with_context(|| format!("type `{name}` missing from schema"))?;
    if ty.get("kind").and_then(Value::as_str) == Some("ENUM") {
        let values: Vec<Value> = ty
            .get("enumValues")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.get("name").cloned())
            .collect();
        return Ok(json!({"type": "string", "enum": values}));
    }
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in ty
        .get("inputFields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let fname = field
            .get("name")
            .and_then(Value::as_str)
            .with_context(|| format!("`{name}` has an unnamed input field"))?;
        let fty = &field["type"];
        if fty.get("kind").and_then(Value::as_str) == Some("NON_NULL")
            && field.get("defaultValue").is_none_or(Value::is_null)
        {
            required.push(Value::String(fname.to_string()));
        }
        properties.insert(
            fname.to_string(),
            introspection_schema(fty, types, pending)?,
        );
    }
    Ok(json!({"type": "object", "properties": properties, "required": required}))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/surface_table/graphql")
    }

    fn table() -> ApiSurface {
        let schema =
            std::fs::read_to_string(fixture_dir().join("schemas/1.0.0.introspection.json"))
                .unwrap();
        let docs: Vec<String> = ["array_set_state", "delete_key", "info", "vms"]
            .iter()
            .map(|f| {
                std::fs::read_to_string(fixture_dir().join(format!("queries/{f}.graphql"))).unwrap()
            })
            .collect();
        build(&schema, &docs, "nas", &Options::default(), |name, m| {
            (if m { "admin" } else { "" }, false, name == "Info")
        })
        .unwrap()
    }

    #[test]
    fn ops_carry_document_variables_and_defs() {
        let t = table();
        let names: Vec<_> = t.ops.iter().map(|o| o.tool.as_str()).collect();
        assert_eq!(
            names,
            [
                "nas.array_set_state",
                "nas.delete_key",
                "nas.info",
                "nas.vms"
            ]
        );
        assert!(t.find("nas.info").unwrap().internal);
        let del = t.find("nas.delete_key").unwrap();
        assert!(del.path.starts_with("mutation DeleteKey"));
        let p: Vec<_> = del
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.wire.as_deref(), p.required))
            .collect();
        assert_eq!(
            p,
            [
                ("key_id", Some("keyId"), true),
                ("type_", Some("type"), false)
            ]
        );
        assert_eq!(del.policy.role, "admin");
        let keys: Vec<_> = t.defs.keys().cloned().collect();
        assert_eq!(
            keys,
            ["ArrayState", "ArrayStateInput", "KeyType", "VmFilter"]
        );
        assert_eq!(t.defs["VmFilter"]["required"], json!([]));
        assert_eq!(
            t.defs["ArrayStateInput"]["required"],
            json!(["desiredState"])
        );
    }

    #[test]
    fn documents_include_fragments_and_validate() {
        let t = table();
        let vms = t.find("nas.vms").unwrap();
        assert!(vms.path.contains("fragment VmFields"));
        assert!(
            t.validate_args(vms, &json!({"filter": {"nameLike": "a", "limit": null}}))
                .is_ok()
        );
        assert!(
            t.validate_args(vms, &json!({"filter": {"limit": "x"}}))
                .is_err()
        );
        let set = t.find("nas.array_set_state").unwrap();
        assert!(
            t.validate_args(set, &json!({"input": {"desiredState": "START"}}))
                .is_ok()
        );
        assert!(
            t.validate_args(set, &json!({"input": {"desiredState": "NOPE"}}))
                .is_err()
        );
    }

    #[test]
    fn unknown_rename_fails() {
        let schema =
            std::fs::read_to_string(fixture_dir().join("schemas/1.0.0.introspection.json"))
                .unwrap();
        let docs =
            vec![std::fs::read_to_string(fixture_dir().join("queries/info.graphql")).unwrap()];
        let mut opts = Options::default();
        opts.renames.insert("info".into(), "about".into());
        let t = build(&schema, &docs, "nas", &opts, |_, _| ("", false, false)).unwrap();
        assert!(t.find("nas.about").is_some());
        opts.renames.insert("missing".into(), "x".into());
        assert!(build(&schema, &docs, "nas", &opts, |_, _| ("", false, false)).is_err());
    }
}
