//! OpenAPI → data-driven descriptor-table generator.
//!
//! The counterpart to [`crate::surface::openapi`]. Where that pass compiles one
//! `#[orca_tool]` fn + a `JsonSchema`-anchored Rust type per operation (24 MB of
//! monomorphized code for Proxmox VE), this pass emits the same surface as
//! **data**: a `&[plugin_toolkit::descriptor::EndpointDescriptor]` table plus one
//! shared `#/components/schemas` blob, both as `&'static str` JSON rodata. The
//! plugin runs the whole surface through `plugin_toolkit::descriptor::execute`,
//! linking neither progenitor nor a compiled type per operation.
//!
//! Run from a plugin `build.rs`:
//! ```rust,ignore
//! plugin_toolkit_build::descriptor::generate(&specs_dir, &out_dir, "proxmox")?;
//! ```
//! Emits `<out_dir>/<flavor>_descriptors.rs`, exposing `pub static TABLE` and
//! `pub static DESCRIPTORS`. The plugin `include!`s it.
#![allow(clippy::disallowed_types)] // raw OpenAPI tree — no fixed struct to model.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

const SPEC_SUFFIXES: &[&str] = &[".openapi.json", ".openapi.yaml", ".openapi.yml"];
const METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

/// One emitted operation, pre-rendered to Rust-source-ready strings.
struct Descriptor {
    name: String,
    description: String,
    method: String,
    path_template: String,
    params: Vec<Param>,
    input_schema: String,
    output_schema: String,
    role: &'static str,
    data_mutation: bool,
}

struct Param {
    name: String,
    loc: &'static str, // "Path" | "Query" | "Body" | "Header"
    required: bool,
}

/// Generate `<out_dir>/<flavor>_descriptors.rs` from `<specs_dir>/<flavor>.openapi.*`.
pub fn generate(specs_dir: &Path, out_dir: &Path, flavor: &str) -> Result<()> {
    println!("cargo:rerun-if-changed={}", specs_dir.display());
    let spec_path = find_spec(specs_dir, flavor)
        .with_context(|| format!("no {flavor}.openapi.* under {}", specs_dir.display()))?;
    println!("cargo:rerun-if-changed={}", spec_path.display());
    let raw = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("read {}", spec_path.display()))?;
    let spec: Value = if raw.trim_start().starts_with('{') {
        serde_json::from_str(&raw).with_context(|| format!("parse {flavor} as JSON"))?
    } else {
        utils::yaml::from_str(&raw).with_context(|| format!("parse {flavor} as YAML"))?
    };

    let defs = spec
        .get("components")
        .and_then(|c| c.get("schemas"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    let descriptors = collect(&spec, flavor);
    let src = emit(&descriptors, &defs, flavor)?;
    let out = out_dir.join(format!("{flavor}_descriptors.rs"));
    std::fs::write(&out, src).with_context(|| format!("write {}", out.display()))?;
    println!(
        "cargo:warning=descriptor[{flavor}]: {} operation(s) emitted as data, {} shared schema(s)",
        descriptors.len(),
        defs.as_object().map(|m| m.len()).unwrap_or(0),
    );
    Ok(())
}

fn find_spec(specs_dir: &Path, flavor: &str) -> Option<std::path::PathBuf> {
    SPEC_SUFFIXES
        .iter()
        .map(|s| specs_dir.join(format!("{flavor}{s}")))
        .find(|p| p.exists())
}

/// Walk every path/method operation into a [`Descriptor`].
fn collect(spec: &Value, flavor: &str) -> Vec<Descriptor> {
    let mut out = Vec::new();
    let Some(paths) = spec.get("paths").and_then(Value::as_object) else {
        return out;
    };
    let mut used = std::collections::HashSet::new();
    for (path, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        // Path-level params apply to every method on the path.
        let shared_params = item.get("parameters").cloned().unwrap_or(Value::Null);
        for &method in METHODS {
            let Some(op) = item.get(method) else { continue };
            let name = tool_name(flavor, method, path, op, &mut used);
            out.push(build(flavor, &name, method, path, op, &shared_params));
        }
    }
    out
}

fn build(
    _flavor: &str,
    name: &str,
    method: &str,
    path: &str,
    op: &Value,
    shared_params: &Value,
) -> Descriptor {
    let mut params = Vec::new();
    // Args-schema properties: `endpoint` is always present.
    let mut props = serde_json::Map::new();
    props.insert(
        "endpoint".into(),
        serde_json::json!({ "type": "string", "description": "Configured endpoint name." }),
    );
    let mut required = vec![Value::String("endpoint".into())];

    for src in [shared_params, op.get("parameters").unwrap_or(&Value::Null)] {
        let Some(list) = src.as_array() else { continue };
        for p in list {
            let (Some(pname), Some(loc)) = (
                p.get("name").and_then(Value::as_str),
                p.get("in").and_then(Value::as_str),
            ) else {
                continue;
            };
            let loc_tag = match loc {
                "path" => "Path",
                "query" => "Query",
                "header" => "Header",
                _ => continue, // cookie params unsupported
            };
            let req = p
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(loc == "path");
            let schema = p.get("schema").cloned().unwrap_or(serde_json::json!({}));
            props.insert(
                pname.to_string(),
                with_description(schema, p.get("description")),
            );
            if req {
                required.push(Value::String(pname.to_string()));
            }
            params.push(Param {
                name: pname.to_string(),
                loc: loc_tag,
                required: req,
            });
        }
    }

    // Request body → body params. Expand object properties into individual body
    // members when the schema is a concrete object; otherwise fall back to a
    // single `body` member carrying the whole payload.
    if let Some(body_schema) = request_body_schema(op) {
        match body_schema.get("properties").and_then(Value::as_object) {
            Some(bprops) => {
                let breq: std::collections::HashSet<&str> = body_schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                for (bname, bschema) in bprops {
                    let req = breq.contains(bname.as_str());
                    props.insert(bname.clone(), bschema.clone());
                    if req {
                        required.push(Value::String(bname.clone()));
                    }
                    params.push(Param {
                        name: bname.clone(),
                        loc: "Body",
                        required: req,
                    });
                }
            }
            None => {
                props.insert("body".into(), body_schema.clone());
                params.push(Param {
                    name: "body".into(),
                    loc: "Body",
                    required: true,
                });
                required.push(Value::String("body".into()));
            }
        }
    }

    let input_schema = serde_json::json!({
        "type": "object",
        "properties": Value::Object(props),
        "required": required,
    })
    .to_string();

    let output_schema = response_schema(op)
        .unwrap_or_else(|| serde_json::json!({}))
        .to_string();

    let mutation = method != "get";
    let user_callable = op
        .get("x-orca-user-callable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let role = if !mutation || user_callable {
        "read"
    } else {
        "admin"
    };

    Descriptor {
        name: name.to_string(),
        description: op
            .get("summary")
            .or_else(|| op.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
        method: method.to_ascii_uppercase(),
        path_template: path.to_string(),
        params,
        input_schema,
        output_schema,
        role,
        data_mutation: mutation,
    }
}

fn with_description(mut schema: Value, desc: Option<&Value>) -> Value {
    if let (Value::Object(m), Some(d)) = (&mut schema, desc)
        && !m.contains_key("description")
    {
        m.insert("description".into(), d.clone());
    }
    schema
}

/// The `application/json` schema of an operation's request body, if any.
fn request_body_schema(op: &Value) -> Option<&Value> {
    op.get("requestBody")?
        .get("content")?
        .get("application/json")?
        .get("schema")
}

/// The `application/json` schema of the first successful (2xx) or `default`
/// response, if any.
fn response_schema(op: &Value) -> Option<Value> {
    let responses = op.get("responses")?.as_object()?;
    let pick = responses
        .iter()
        .find(|(k, _)| k.starts_with('2'))
        .or_else(|| responses.iter().find(|(k, _)| *k == "default"))?;
    pick.1
        .get("content")
        .and_then(|c| c.get("application/json"))
        .and_then(|j| j.get("schema"))
        .cloned()
}

/// `{flavor}.{operation_id}` — deduped, sanitized to a valid tool-name tail.
fn tool_name(
    flavor: &str,
    method: &str,
    path: &str,
    op: &Value,
    used: &mut std::collections::HashSet<String>,
) -> String {
    let base = op
        .get("operationId")
        .and_then(Value::as_str)
        .map(sanitize)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| derive_name(method, path));
    let mut name = format!("{flavor}.{base}");
    let mut n = 2;
    while !used.insert(name.clone()) {
        name = format!("{flavor}.{base}_{n}");
        n += 1;
    }
    name
}

/// Derive a name from method + path when there is no operationId:
/// `GET /nodes/{node}/status` → `get_nodes_node_status`.
fn derive_name(method: &str, path: &str) -> String {
    let mut s = method.to_ascii_lowercase();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        s.push('_');
        s.push_str(&sanitize(seg));
    }
    s
}

fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us && !out.is_empty() {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Render the descriptor table to Rust source. Schemas are embedded as
/// Rust-escaped string literals (via `{:?}`) so any JSON — quotes, backslashes,
/// unicode — round-trips without a raw-string-fence collision.
fn emit(descriptors: &[Descriptor], defs: &Value, flavor: &str) -> Result<String> {
    let module = flavor.to_ascii_uppercase();
    let mut s = String::new();
    s.push_str(&format!(
        "// @generated by plugin_toolkit_build::descriptor — do not edit.\n\
         #[allow(clippy::all)]\n\
         pub static {module}_DESCRIPTORS: &[::plugin_toolkit::descriptor::EndpointDescriptor] = &[\n"
    ));
    for d in descriptors {
        writeln!(
            s,
            "    ::plugin_toolkit::descriptor::EndpointDescriptor {{\n\
             \x20       name: {name:?},\n\
             \x20       description: {desc:?},\n\
             \x20       method: {method:?},\n\
             \x20       path_template: {path:?},\n\
             \x20       params: &[{params}],\n\
             \x20       input_schema: {input:?},\n\
             \x20       output_schema: {output:?},\n\
             \x20       remote_ok: false,\n\
             \x20       required_role: {role:?},\n\
             \x20       data_mutation: {mutation},\n\
             \x20   }},",
            name = d.name,
            desc = d.description,
            method = d.method,
            path = d.path_template,
            params = render_params(&d.params),
            input = d.input_schema,
            output = d.output_schema,
            role = d.role,
            mutation = d.data_mutation,
        )?;
    }
    s.push_str("];\n\n");

    // Shared component-schema pool, compact JSON in one string literal.
    let defs_json = serde_json::to_string(defs)?;
    writeln!(s, "pub static {module}_DEFS: &str = {defs_json:?};\n")?;
    writeln!(
        s,
        "pub static {module}_TABLE: ::plugin_toolkit::descriptor::DescriptorTable =\n    \
         ::plugin_toolkit::descriptor::DescriptorTable::new({module}_DESCRIPTORS, {module}_DEFS);"
    )?;
    Ok(s)
}

fn render_params(params: &[Param]) -> String {
    params
        .iter()
        .map(|p| {
            format!(
                "::plugin_toolkit::descriptor::ParamSpec {{ name: {:?}, loc: ::plugin_toolkit::descriptor::ParamLoc::{}, required: {} }}",
                p.name, p.loc, p.required
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tiny_spec() -> Value {
        json!({
            "openapi": "3.0.0",
            "paths": {
                "/nodes/{node}/status": {
                    "parameters": [
                        { "name": "node", "in": "path", "required": true, "schema": { "type": "string" } }
                    ],
                    "get": {
                        "operationId": "get_node_status",
                        "summary": "Node status",
                        "responses": {
                            "200": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Status" } } } }
                        }
                    }
                },
                "/vms": {
                    "post": {
                        "operationId": "create_vm",
                        "requestBody": { "content": { "application/json": { "schema": {
                            "type": "object", "required": ["vmid"],
                            "properties": { "vmid": { "type": "integer" }, "name": { "type": "string" } }
                        } } } },
                        "responses": { "200": {} }
                    }
                }
            },
            "components": { "schemas": { "Status": { "type": "object" } } }
        })
    }

    #[test]
    fn collects_get_with_path_param_and_response_ref() {
        let ds = collect(&tiny_spec(), "proxmox");
        let get = ds
            .iter()
            .find(|d| d.name == "proxmox.get_node_status")
            .unwrap();
        assert_eq!(get.method, "GET");
        assert_eq!(get.role, "read");
        assert!(!get.data_mutation);
        assert!(
            get.params
                .iter()
                .any(|p| p.name == "node" && p.loc == "Path" && p.required)
        );
        assert!(get.output_schema.contains("$ref"));
        // Input schema always carries `endpoint`.
        assert!(get.input_schema.contains("endpoint"));
    }

    #[test]
    fn collects_post_body_props_as_body_params() {
        let ds = collect(&tiny_spec(), "proxmox");
        let post = ds.iter().find(|d| d.name == "proxmox.create_vm").unwrap();
        assert_eq!(post.method, "POST");
        assert_eq!(post.role, "admin");
        assert!(post.data_mutation);
        assert!(
            post.params
                .iter()
                .any(|p| p.name == "vmid" && p.loc == "Body" && p.required)
        );
        assert!(
            post.params
                .iter()
                .any(|p| p.name == "name" && p.loc == "Body" && !p.required)
        );
    }

    /// Measurement harness against a real spec. Run with:
    /// `PROXMOX_SPEC=/abs/path/proxmox.openapi.json cargo test -p plugin-toolkit-build \
    ///   descriptor::tests::measure_real_spec -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn measure_real_spec() {
        let path = std::env::var("PROXMOX_SPEC").expect("set PROXMOX_SPEC");
        let raw = std::fs::read_to_string(&path).unwrap();
        let spec: Value = serde_json::from_str(&raw).unwrap();
        let defs = spec
            .get("components")
            .and_then(|c| c.get("schemas"))
            .cloned()
            .unwrap_or(json!({}));
        let ds = collect(&spec, "proxmox");
        let src = emit(&ds, &defs, "proxmox").unwrap();
        syn::parse_file(&src).expect("real emitted table must be valid Rust");
        let defs_json = serde_json::to_string(&defs).unwrap();
        eprintln!("operations:        {}", ds.len());
        eprintln!(
            "shared schemas:    {}",
            defs.as_object().map(|m| m.len()).unwrap_or(0)
        );
        eprintln!("DEFS blob bytes:   {}", defs_json.len());
        eprintln!("generated .rs KB:  {}", src.len() / 1024);
        std::fs::write("/tmp/proxmox_descriptors.rs", &src).unwrap();
    }

    #[test]
    fn emitted_source_is_parseable_rust() {
        let ds = collect(&tiny_spec(), "proxmox");
        let defs = tiny_spec()
            .get("components")
            .unwrap()
            .get("schemas")
            .unwrap()
            .clone();
        let src = emit(&ds, &defs, "proxmox").unwrap();
        syn::parse_file(&src).expect("emitted descriptor table must be valid Rust");
        assert!(src.contains("PROXMOX_TABLE"));
        assert!(src.contains("PROXMOX_DEFS"));
    }
}
