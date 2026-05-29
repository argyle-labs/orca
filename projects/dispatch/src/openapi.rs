//! OpenAPI registration for every `#[orca_tool]`.
//!
//! Counterpart to `ToolRegistration` (which drives MCP/REST dispatch and CLI):
//! every annotated tool *also* submits an `OpenApiToolRegistration` so the
//! generated OpenAPI 3.1 spec includes a `POST /api/tools/<name>` entry with
//! the Args request body schema and the Output 200-response schema — both
//! derived from schemars JSON Schema (Draft 2020-12, native to OpenAPI 3.1).

#![allow(clippy::disallowed_types)] // OpenAPI spec construction is dynamic JSON

use serde_json::{Map, Value, json};

/// One entry per `#[orca_tool]`. The macro fills `args_schema` / `output_schema`
/// with thunks that call `schemars::schema_for!(T)` lazily so we don't pay the
/// cost unless someone actually emits the spec.
pub struct OpenApiToolRegistration {
    pub name: &'static str,
    pub description: &'static str,
    pub domain: &'static str,
    pub args_schema: fn() -> Value,
    pub output_schema: fn() -> Value,
}

inventory::collect!(OpenApiToolRegistration);

/// Walk every inventory entry and inject a `POST /api/tools/<name>` path into
/// the given spec value. Mutates `spec` in place.
pub fn inject_tool_paths(spec: &mut Value) {
    let Some(obj) = spec.as_object_mut() else {
        return;
    };

    // OpenAPI 3.1 + 2020-12 schema dialect. Schemars 1.x emits 2020-12 natively.
    obj.insert("openapi".to_string(), Value::String("3.1.0".to_string()));
    obj.insert(
        "jsonSchemaDialect".to_string(),
        Value::String("https://json-schema.org/draft/2020-12/schema".to_string()),
    );

    let mut new_paths: Map<String, Value> = Map::new();
    let mut hoisted_defs: Map<String, Value> = Map::new();
    let mut tags_seen = std::collections::BTreeSet::<String>::new();

    for entry in inventory::iter::<OpenApiToolRegistration> {
        let path = format!("/api/tools/{}", entry.name);
        let mut args_schema = (entry.args_schema)();
        let mut output_schema = (entry.output_schema)();
        let domain = entry.domain.to_string();
        tags_seen.insert(domain.clone());

        hoist_defs(&mut args_schema, &mut hoisted_defs);
        hoist_defs(&mut output_schema, &mut hoisted_defs);
        rewrite_refs(&mut args_schema);
        rewrite_refs(&mut output_schema);
        strip_meta(&mut args_schema);
        strip_meta(&mut output_schema);

        let path_item = json!({
            "post": {
                "operationId": operation_id_for(entry.name),
                "summary": entry.description,
                "description": entry.description,
                "tags": [domain],
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": { "schema": args_schema }
                    }
                },
                "responses": {
                    "200": {
                        "description": "Tool result",
                        "content": {
                            "application/json": { "schema": output_schema }
                        }
                    },
                    "404": tool_error_response("Unknown tool"),
                    "500": tool_error_response("Tool execution failed"),
                }
            }
        });

        new_paths.insert(path, path_item);
    }

    // Rewrite refs inside hoisted defs themselves (they can reference each other).
    for v in hoisted_defs.values_mut() {
        rewrite_refs(v);
        strip_meta(v);
    }

    // Merge new paths into the spec's paths object.
    let paths = obj
        .entry("paths".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(paths_obj) = paths.as_object_mut() {
        for (k, v) in new_paths {
            paths_obj.insert(k, v);
        }
    }

    // Merge hoisted defs into components.schemas.
    let components = obj
        .entry("components".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(components_obj) = components.as_object_mut() {
        let schemas = components_obj
            .entry("schemas".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(schemas_obj) = schemas.as_object_mut() {
            for (k, v) in hoisted_defs {
                // Don't clobber utoipa-registered schemas.
                schemas_obj.entry(k).or_insert(v);
            }
        }
    }

    // Append any new domain tags so generated SDKs group methods correctly.
    if !tags_seen.is_empty() {
        let tags = obj
            .entry("tags".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(arr) = tags.as_array_mut() {
            let existing: std::collections::BTreeSet<String> = arr
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect();
            for d in &tags_seen {
                if !existing.contains(d) {
                    arr.push(json!({ "name": d }));
                }
            }
        }
    }
}

fn tool_error_response(desc: &str) -> Value {
    json!({
        "description": desc,
        "content": {
            "application/json": {
                "schema": {
                    "type": "object",
                    "properties": { "error": { "type": "string" } },
                    "required": ["error"]
                }
            }
        }
    })
}

/// Pull `$defs` out of `schema` into the shared `out` map. Keeps schemars'
/// definition names (they're already PascalCase and stable).
fn hoist_defs(schema: &mut Value, out: &mut Map<String, Value>) {
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    if let Some(Value::Object(defs)) = obj.remove("$defs") {
        for (name, def) in defs {
            out.entry(name).or_insert(def);
        }
    }
}

/// Rewrite all `$ref: "#/$defs/X"` → `$ref: "#/components/schemas/X"`,
/// recursively, in place.
fn rewrite_refs(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(s)) = map.get_mut("$ref")
                && let Some(rest) = s.strip_prefix("#/$defs/")
            {
                *s = format!("#/components/schemas/{rest}");
            }
            for child in map.values_mut() {
                rewrite_refs(child);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                rewrite_refs(child);
            }
        }
        _ => {}
    }
}

/// Strip JSON-Schema-only meta keys that some OpenAPI tooling rejects
/// (notably `$schema` at the root of a schema object).
fn strip_meta(v: &mut Value) {
    if let Value::Object(map) = v {
        map.remove("$schema");
    }
}

/// `engine.list` → `engineList`. camelCase the dotted tool name so hey-api
/// generates an idiomatic JS method name per tool.
fn operation_id_for(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper = false;
    for c in name.chars() {
        match c {
            '.' | '_' | '-' => upper = true,
            _ => {
                if upper {
                    out.extend(c.to_uppercase());
                    upper = false;
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_id_camelcases_dotted_names() {
        assert_eq!(operation_id_for("engine.list"), "engineList");
        assert_eq!(operation_id_for("pod.cert_status"), "podCertStatus");
        assert_eq!(operation_id_for("host.info"), "hostInfo");
    }

    #[test]
    fn inject_sets_3_1_and_dialect() {
        let mut spec = json!({ "openapi": "3.0.3", "paths": {} });
        inject_tool_paths(&mut spec);
        assert_eq!(spec["openapi"], "3.1.0");
        assert!(spec["jsonSchemaDialect"].is_string());
    }

    #[test]
    fn rewrite_refs_handles_nested() {
        let mut v = json!({
            "properties": {
                "child": { "$ref": "#/$defs/Foo" },
                "list": [{ "$ref": "#/$defs/Bar" }]
            }
        });
        rewrite_refs(&mut v);
        assert_eq!(v["properties"]["child"]["$ref"], "#/components/schemas/Foo");
        assert_eq!(
            v["properties"]["list"][0]["$ref"],
            "#/components/schemas/Bar"
        );
    }

    #[test]
    fn hoist_defs_moves_and_clears() {
        let mut schema = json!({ "type": "object", "$defs": { "Foo": { "type": "string" } } });
        let mut out = Map::new();
        hoist_defs(&mut schema, &mut out);
        assert!(schema.get("$defs").is_none());
        assert_eq!(out.get("Foo").unwrap(), &json!({ "type": "string" }));
    }
}
