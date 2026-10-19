#![allow(clippy::disallowed_types)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ::openapi::surface::{
    ApiOp, ApiSurface, Auth, Base, EndpointBinding, FORMAT, OpBody, OpOutput, Param, ParamIn,
    Policy, ResponseRules, SurfaceKind,
};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::surface::openapi::{
    MarkerMap, Markers, check_marker_usage, is_write, markers_from_spec_value, tool_policy,
};

const METHODS: &[&str] = &["get", "put", "post", "delete", "patch"];
const BODY_PARAM: &str = "body";

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub api_root: String,
    pub auth: Option<Auth>,
    pub response: ResponseRules,
    pub endpoint: EndpointBinding,
    pub read_allowlist: Option<PathBuf>,
    pub gated_gets: Option<PathBuf>,
    pub renames: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct GetLists {
    pub allow: BTreeSet<String>,
    pub gated: BTreeSet<String>,
}

pub fn generate(
    spec_path: &Path,
    out_dir: &Path,
    surface: &str,
    opts: &Options,
) -> Result<PathBuf> {
    println!("cargo:rerun-if-changed={}", spec_path.display());
    let raw = std::fs::read_to_string(spec_path)
        .with_context(|| format!("read {}", spec_path.display()))?;
    let lists = match (&opts.read_allowlist, &opts.gated_gets) {
        (None, None) => None,
        (allow, gated) => Some(GetLists {
            allow: read_list(allow.as_deref())?,
            gated: read_list(gated.as_deref())?,
        }),
    };
    let (table, warnings) = build(&raw, surface, opts, lists.as_ref())?;
    for w in warnings {
        println!("cargo:warning=surface_table[{surface}]: {w}");
    }
    super::write(&table, out_dir)
}

pub fn parse_list(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn read_list(path: Option<&Path>) -> Result<BTreeSet<String>> {
    let Some(path) = path else {
        return Ok(BTreeSet::new());
    };
    println!("cargo:rerun-if-changed={}", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(parse_list(&text))
}

pub fn build(
    raw: &str,
    surface: &str,
    opts: &Options,
    lists: Option<&GetLists>,
) -> Result<(ApiSurface, Vec<String>)> {
    let mut value = crate::openapi::parse_spec_value(raw, surface)?;
    let mut markers = MarkerMap::new();
    markers_from_spec_value(&value, &mut markers)?;
    if ::openapi::lower_31::is_31(&value) {
        ::openapi::lower_31::lower_to_30(&mut value)
            .with_context(|| format!("lower 3.1 -> 3.0 for {surface}"))?
            .emit_cargo_warnings(surface);
    }
    let non_json = non_json_responses(&value)?;
    let mut typed: openapiv3::OpenAPI =
        serde_json::from_value(value).with_context(|| format!("parse {surface} as openapiv3"))?;
    ::openapi::normalize::for_progenitor(&mut typed).emit_cargo_warnings(surface);
    let spec = serde_json::to_value(&typed).context("re-serialize normalized spec")?;
    let components = spec.get("components").cloned().unwrap_or(Value::Null);

    let mut warnings = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ops: Vec<(ApiOp, Markers)> = Vec::new();
    let mut raw_responses = 0usize;
    let empty = Map::new();
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for (path, item) in paths {
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            let http = method.to_ascii_uppercase();
            seen.insert((http.clone(), path.clone()));
            if non_json.contains(&(http.clone(), path.clone())) {
                raw_responses += 1;
                continue;
            }
            let output = output_schema(op, &components)?;
            let op_id = op
                .get("operationId")
                .and_then(Value::as_str)
                .with_context(|| format!("{http} {path}: no operationId"))?;
            let mk = markers
                .get(&(http.clone(), path.clone()))
                .copied()
                .unwrap_or_default();
            let tail = progenitor_ident(op_id);
            let params =
                op_params(item, op, &components).with_context(|| format!("{http} {path}"))?;
            let (params, body) =
                with_body(params, op, &components).with_context(|| format!("{http} {path}"))?;
            ops.push((
                ApiOp {
                    tool: tail.clone(),
                    method: http.clone(),
                    path: path.clone(),
                    params,
                    body,
                    output: output.map(|schema| OpOutput { schema }),
                    policy: derived_policy(&tail, &http, mk),
                    internal: mk.no_tool,
                },
                mk,
            ));
        }
    }
    check_marker_usage(&markers, &seen, surface)?;
    if raw_responses > 0 {
        warnings.push(format!(
            "{raw_responses} op(s) with a non-JSON response omitted"
        ));
    }
    if let Some(lists) = lists {
        classify_gets(&mut ops, lists, &mut warnings)?;
    }
    apply_renames(&mut ops, &opts.renames)?;

    let mut names = BTreeSet::new();
    let mut out: Vec<ApiOp> = Vec::with_capacity(ops.len());
    for (mut op, _) in ops {
        op.tool = format!("{surface}.{}", op.tool);
        if !names.insert(op.tool.clone()) {
            bail!("duplicate tool `{}`", op.tool);
        }
        out.push(op);
    }
    out.sort_by(|a, b| a.tool.cmp(&b.tool));

    let defs = schema_defs(&out, &components)?;
    Ok((
        ApiSurface {
            format: FORMAT.to_string(),
            surface: surface.to_string(),
            kind: SurfaceKind::Openapi,
            base: Base {
                api_root: opts.api_root.clone(),
                graphql_path: None,
            },
            auth: opts.auth.clone(),
            response: opts.response.clone(),
            endpoint: opts.endpoint.clone(),
            defs,
            ops: out,
        },
        warnings,
    ))
}

pub(crate) fn progenitor_ident(input: &str) -> String {
    use heck::ToSnakeCase;
    let out = match input {
        "+1" => "plus1".to_string(),
        "-1" => "minus1".to_string(),
        _ => input
            .replace('\'', "")
            .replace(|c: char| !unicode_ident::is_xid_continue(c), "-")
            .to_snake_case(),
    };
    let out = match out.chars().next() {
        None => "x".to_string(),
        Some(c) if unicode_ident::is_xid_start(c) => out,
        Some(_) => format!("_{out}"),
    };
    if typify_impl::accept_as_ident(&out) {
        out
    } else {
        format!("{out}_")
    }
}

fn derived_policy(tail: &str, http: &str, mk: Markers) -> Policy {
    let (role, data_mutation) = tool_policy(is_write(http), mk);
    let read_shaped = verb_shape::is_read_shaped(tail);
    let role = match role {
        "" if read_shaped => "read",
        "" => "admin",
        declared => declared,
    };
    Policy {
        role: role.to_string(),
        execute_gated: !read_shaped,
        data_mutation,
        local_only: false,
    }
}

fn classify_gets(
    ops: &mut [(ApiOp, Markers)],
    lists: &GetLists,
    warnings: &mut Vec<String>,
) -> Result<()> {
    if let Some(both) = lists.allow.intersection(&lists.gated).next() {
        bail!("{both} is both read-allowlisted and gated");
    }
    let mut unused: BTreeSet<&String> = lists.allow.union(&lists.gated).collect();
    for (op, mk) in ops.iter_mut().filter(|(op, _)| op.method == "GET") {
        unused.remove(&op.path);
        let allowed = lists.allow.contains(&op.path);
        if allowed && mk.admin {
            bail!(
                "GET {} is x-orca-admin in the spec and read-allowlisted",
                op.path
            );
        }
        if allowed && op.internal {
            bail!(
                "GET {} is x-orca-no-tool in the spec and read-allowlisted",
                op.path
            );
        }
        let gated = lists.gated.contains(&op.path);
        if gated && op.internal {
            warnings.push(format!(
                "gated GET {} has no tool (x-orca-no-tool); gate kept",
                op.path
            ));
        }
        op.policy = Policy {
            role: if allowed { "read" } else { "admin" }.to_string(),
            execute_gated: gated,
            data_mutation: false,
            local_only: false,
        };
    }
    if let Some(path) = unused.first() {
        bail!("read allowlist / gated list name a path with no GET: {path}");
    }
    Ok(())
}

fn apply_renames(ops: &mut [(ApiOp, Markers)], renames: &BTreeMap<String, String>) -> Result<()> {
    let mut unused: BTreeSet<&String> = renames.keys().collect();
    for (op, _) in ops.iter_mut() {
        if let Some(to) = renames.get(&op.tool) {
            unused.remove(&op.tool);
            op.tool = to.clone();
        }
    }
    if let Some(from) = unused.first() {
        bail!("rename `{from}` matches no operation");
    }
    Ok(())
}

fn resolve<'a>(components: &'a Value, v: &'a Value) -> Result<&'a Value> {
    let Some(r) = v.get("$ref").and_then(Value::as_str) else {
        return Ok(v);
    };
    r.strip_prefix("#/components/")
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(section, name)| components.get(section)?.get(name))
        .with_context(|| format!("unresolved $ref {r}"))
}

fn op_params(item: &Value, op: &Value, components: &Value) -> Result<Vec<Param>> {
    let mut by_name: BTreeMap<String, &Value> = BTreeMap::new();
    for list in [item.get("parameters"), op.get("parameters")] {
        for p in list.and_then(Value::as_array).into_iter().flatten() {
            let p = resolve(components, p)?;
            let name = p
                .get("name")
                .and_then(Value::as_str)
                .context("parameter without a name")?;
            by_name.insert(name.to_string(), p);
        }
    }
    let mut out = Vec::with_capacity(by_name.len());
    for (wire, p) in by_name {
        let location = match p.get("in").and_then(Value::as_str) {
            Some("path") => ParamIn::Path,
            Some("query") => ParamIn::Query,
            Some("header") => ParamIn::Header,
            other => bail!("parameter `{wire}`: unsupported location {other:?}"),
        };
        let name = progenitor_ident(&wire);
        out.push(Param {
            wire: (name != wire).then_some(wire),
            name,
            required: location == ParamIn::Path
                || p.get("required").and_then(Value::as_bool).unwrap_or(false),
            location,
            schema: p
                .get("schema")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
        });
    }
    Ok(out)
}

fn with_body(
    mut params: Vec<Param>,
    op: &Value,
    components: &Value,
) -> Result<(Vec<Param>, Option<OpBody>)> {
    let Some(body) = op.get("requestBody") else {
        return Ok((params, None));
    };
    let body = resolve(components, body)?;
    let Some((content_type, media)) = body
        .get("content")
        .and_then(Value::as_object)
        .and_then(|c| c.iter().next())
    else {
        return Ok((params, None));
    };
    if params.iter().any(|p| p.name == BODY_PARAM) {
        bail!("a parameter named `{BODY_PARAM}` collides with the request body");
    }
    params.push(Param {
        name: BODY_PARAM.to_string(),
        location: ParamIn::Body,
        wire: None,
        required: true,
        schema: media
            .get("schema")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())),
    });
    Ok((
        params,
        Some(OpBody {
            schema: Value::Object(Map::new()),
            content_type: content_type.clone(),
            param: Some(BODY_PARAM.to_string()),
        }),
    ))
}

fn success_content<'a>(
    op: &'a Value,
    components: &'a Value,
) -> Result<Option<&'a Map<String, Value>>> {
    let Some((_, response)) = op
        .get("responses")
        .and_then(Value::as_object)
        .and_then(|r| r.iter().find(|(code, _)| code.starts_with('2')))
    else {
        return Ok(None);
    };
    Ok(resolve(components, response)?
        .get("content")
        .and_then(Value::as_object)
        .filter(|c| !c.is_empty()))
}

fn json_media(content: &Map<String, Value>) -> Option<&Value> {
    content
        .iter()
        .find(|(ct, _)| *ct == "application/json" || ct.starts_with("application/json;"))
        .map(|(_, media)| media)
}

fn non_json_responses(spec: &Value) -> Result<BTreeSet<(String, String)>> {
    let components = spec.get("components").unwrap_or(&Value::Null);
    let mut out = BTreeSet::new();
    for (path, item) in spec
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for method in METHODS {
            let Some(op) = item.get(*method) else {
                continue;
            };
            if success_content(op, components)?.is_some_and(|c| json_media(c).is_none()) {
                out.insert((method.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    Ok(out)
}

fn output_schema(op: &Value, components: &Value) -> Result<Option<Value>> {
    Ok(success_content(op, components)?
        .and_then(json_media)
        .and_then(|media| media.get("schema").cloned()))
}

fn schema_defs(ops: &[ApiOp], components: &Value) -> Result<Map<String, Value>> {
    let roots: Vec<&Value> = ops
        .iter()
        .flat_map(|op| {
            op.params
                .iter()
                .map(|p| &p.schema)
                .chain(op.output.iter().map(|o| &o.schema))
        })
        .collect();
    let root = serde_json::to_value(roots).context("collect op schemas")?;
    let schemas = components.get("schemas");
    Ok(crate::prune::closure(&root, components)?
        .into_iter()
        .filter(|(section, _)| section == "schemas")
        .filter_map(|(_, name)| {
            let def = schemas?.get(&name)?.clone();
            Some((name, def))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec() -> String {
        json!({
            "openapi": "3.0.3",
            "info": {"title": "t", "version": "1"},
            "paths": {
                "/nodes/{node}/qemu": {
                    "get": {
                        "operationId": "listVMs",
                        "parameters": [
                            {"name": "node", "in": "path", "required": true, "schema": {"type": "string"}},
                            {"name": "full-list", "in": "query", "schema": {"type": "boolean"}},
                            {"name": "type", "in": "query", "required": true, "schema": {"type": "string"}}
                        ],
                        "responses": {"200": {"description": "ok", "content": {"application/json": {
                            "schema": {"type": "array", "items": {"$ref": "#/components/schemas/Vm"}}
                        }}}}
                    },
                    "post": {
                        "operationId": "create-vm",
                        "x-orca-user-callable": true,
                        "parameters": [{"name": "node", "in": "path", "required": true, "schema": {"type": "string"}}],
                        "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/NewVm"}}}},
                        "responses": {"200": {"description": "ok"}}
                    }
                },
                "/secrets": {
                    "get": {
                        "operationId": "secrets",
                        "x-orca-no-tool": true,
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"type": "object"}}}}}
                    }
                },
                "/files": {
                    "get": {
                        "operationId": "download",
                        "responses": {"200": {"description": "ok", "content": {"application/octet-stream": {"schema": {"type": "string", "format": "binary"}}}}}
                    }
                },
                "/version": {
                    "get": {
                        "operationId": "get",
                        "responses": {"200": {"description": "ok", "content": {"application/json": {"schema": {"type": "object"}}}}}
                    }
                }
            },
            "components": {"schemas": {
                "Vm": {"type": "object", "properties": {"cpu": {"$ref": "#/components/schemas/Cpu"}}},
                "Cpu": {"type": "number"},
                "NewVm": {"type": "object", "properties": {"name": {"type": "string"}}},
                "Unused": {"type": "string"}
            }}
        })
        .to_string()
    }

    fn table(lists: Option<&GetLists>) -> ApiSurface {
        build(&spec(), "pve", &Options::default(), lists).unwrap().0
    }

    #[test]
    fn names_params_and_wire_follow_progenitor() {
        let t = table(None);
        let names: Vec<_> = t.ops.iter().map(|o| o.tool.as_str()).collect();
        assert_eq!(
            names,
            ["pve.create_vm", "pve.get", "pve.list_v_ms", "pve.secrets"]
        );
        let list = t.find("pve.list_v_ms").unwrap();
        let p: Vec<_> = list
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.wire.as_deref(), p.required))
            .collect();
        assert_eq!(
            p,
            [
                ("full_list", Some("full-list"), false),
                ("node", None, true),
                ("type_", Some("type"), true)
            ]
        );
        let create = t.find("pve.create_vm").unwrap();
        assert_eq!(create.params[1].name, "body");
        assert_eq!(create.params[1].location, ParamIn::Body);
        assert_eq!(create.body.as_ref().unwrap().param.as_deref(), Some("body"));
    }

    #[test]
    fn defs_are_the_ref_closure() {
        let keys: Vec<_> = table(None).defs.keys().cloned().collect();
        assert_eq!(keys, ["Cpu", "NewVm", "Vm"]);
    }

    #[test]
    fn policies_follow_markers_and_verb_shape() {
        let t = table(None);
        let create = &t.find("pve.create_vm").unwrap().policy;
        assert_eq!(create.role, "admin");
        assert!(create.execute_gated && create.data_mutation);
        let list = &t.find("pve.list_v_ms").unwrap().policy;
        assert_eq!(list.role, "admin");
        assert!(list.execute_gated && !list.data_mutation);
        let get = &t.find("pve.get").unwrap().policy;
        assert_eq!(get.role, "read");
        assert!(!get.execute_gated);
        assert!(t.find("pve.secrets").unwrap().internal);
    }

    #[test]
    fn get_lists_classify_every_get() {
        let lists = GetLists {
            allow: parse_list("# c\n/nodes/{node}/qemu\n"),
            gated: parse_list("/secrets\n"),
        };
        let (t, warnings) = build(&spec(), "pve", &Options::default(), Some(&lists)).unwrap();
        let list = &t.find("pve.list_v_ms").unwrap().policy;
        assert_eq!((list.role.as_str(), list.execute_gated), ("read", false));
        let version = &t.find("pve.get").unwrap().policy;
        assert_eq!(
            (version.role.as_str(), version.execute_gated),
            ("admin", false)
        );
        let secrets = &t.find("pve.secrets").unwrap().policy;
        assert_eq!(
            (secrets.role.as_str(), secrets.execute_gated),
            ("admin", true)
        );
        assert!(warnings.iter().any(|w| w.contains("gate kept")));
    }

    #[test]
    fn get_list_conflicts_fail() {
        let both = GetLists {
            allow: parse_list("/version"),
            gated: parse_list("/version"),
        };
        assert!(build(&spec(), "pve", &Options::default(), Some(&both)).is_err());
        let unused = GetLists {
            allow: parse_list("/nope"),
            gated: BTreeSet::new(),
        };
        assert!(build(&spec(), "pve", &Options::default(), Some(&unused)).is_err());
        let hidden = GetLists {
            allow: parse_list("/secrets"),
            gated: BTreeSet::new(),
        };
        assert!(build(&spec(), "pve", &Options::default(), Some(&hidden)).is_err());
    }

    #[test]
    fn renames_apply_and_unknown_renames_fail() {
        let mut opts = Options::default();
        opts.renames.insert("list_v_ms".into(), "list_vms".into());
        let t = build(&spec(), "pve", &opts, None).unwrap().0;
        assert!(t.find("pve.list_vms").is_some());
        opts.renames.insert("missing".into(), "x".into());
        assert!(build(&spec(), "pve", &opts, None).is_err());
    }

    #[test]
    fn write_round_trips_compact_json() {
        let dir = std::env::temp_dir().join(format!("surface-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = super::super::write(&table(None), &dir).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains('\n'));
        assert_eq!(ApiSurface::parse(&text).unwrap(), table(None));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
