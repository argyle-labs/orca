//! OpenAPI registration for every `#[orca_tool]`.
//!
//! Counterpart to `ToolRegistration` (which drives MCP/REST dispatch and CLI):
//! every annotated tool *also* submits an `OpenApiToolRegistration` so the
//! generated OpenAPI 3.1 spec includes a `POST /api/v1/<name>` entry with
//! the Args request body schema and the Output 200-response schema — both
//! derived from schemars JSON Schema (Draft 2020-12, native to OpenAPI 3.1).

#![allow(clippy::disallowed_types)] // OpenAPI spec construction is dynamic JSON

use serde_json::{Map, Value, json};

/// One entry per `#[orca_tool]`. The macro fills `args_schema` / `output_schema`
/// with thunks that call `schemars::schema_for!(T)` lazily so we don't pay the
/// cost unless someone actually emits the spec.
pub struct OpenApiToolRegistration {
    pub name: &'static str,
    /// Short human-friendly title from `#[orca_tool(..., title = "...")]`.
    /// `None` when the author didn't set one — fall back to `name` at render
    /// time. Distinct from `description` (the doc-comment markdown body) so
    /// the API reference can show one as the nav label and the other as the
    /// detail panel content.
    pub title: Option<&'static str>,
    pub description: &'static str,
    pub domain: &'static str,
    pub args_schema: fn() -> Value,
    pub output_schema: fn() -> Value,
}

inventory::collect!(OpenApiToolRegistration);

/// Walk every inventory entry and inject a `POST /api/v1/<name>` path into
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

    // Declare the cookie-based session as a security scheme so Scalar's
    // per-operation UI can show a lock icon and explain how to authenticate.
    // The Scalar wrapper page (`render_scalar` in server) renders an inline
    // sign-in widget that calls `/api/auth/web/signin` and lets the browser
    // cookie jar do the rest — every subsequent "try it" request is
    // same-origin so the cookie auto-attaches.
    let components = obj
        .entry("components".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(c) = components.as_object_mut() {
        let schemes = c
            .entry("securitySchemes".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(s) = schemes.as_object_mut() {
            s.insert(
                "cookieAuth".to_string(),
                json!({
                    "type": "apiKey",
                    "in": "cookie",
                    "name": "orca_session",
                    "description": "Browser session cookie issued by `POST /api/auth/web/signin`. Sign in via the widget at the top of this page; the cookie auto-attaches to every same-origin Try-It request."
                }),
            );
        }
    }
    obj.insert(
        "security".to_string(),
        Value::Array(vec![json!({ "cookieAuth": [] })]),
    );

    let mut new_paths: Map<String, Value> = Map::new();
    let mut hoisted_defs: Map<String, Value> = Map::new();
    let mut tags_seen = std::collections::BTreeSet::<String>::new();

    for entry in inventory::iter::<OpenApiToolRegistration> {
        let path = format!("/api/v1/{}", entry.name);
        let mut args_schema = (entry.args_schema)();
        let mut output_schema = (entry.output_schema)();
        // Tag by the ROOT domain only (`auth.session` → `auth`) so every
        // sub-resource collapses into one group in the Scalar nav. The
        // dotted operation name in `summary` already conveys the hierarchy
        // (`auth.session.create` reads as auth → session → create at a
        // glance), so we don't also need x-tagGroups duplicating the work.
        let domain = entry
            .domain
            .split_once('.')
            .map(|(root, _)| root)
            .unwrap_or(entry.domain)
            .to_string();
        tags_seen.insert(domain.clone());

        hoist_defs(&mut args_schema, &mut hoisted_defs);
        hoist_defs(&mut output_schema, &mut hoisted_defs);
        rewrite_refs(&mut args_schema);
        rewrite_refs(&mut output_schema);
        wrap_ref_siblings(&mut args_schema);
        wrap_ref_siblings(&mut output_schema);
        strip_meta(&mut args_schema);
        strip_meta(&mut output_schema);

        // Summary = explicit `title = "..."` from the macro, else fall back
        // to the canonical tool name. Drives the Scalar left-nav label AND
        // the right-pane header — must stay structurally distinct from
        // `description` (the doc-comment body), so we deliberately suppress
        // the description when it would echo the title verbatim (the
        // `no doc comment` case where `entry.description == entry.name`).
        let summary = entry.title.unwrap_or(entry.name);
        let clean_desc_owned: String = entry
            .description
            .strip_prefix("[MUTATES STATE] ")
            .unwrap_or(entry.description)
            .to_string();
        let description_field: Value = if clean_desc_owned == entry.name
            || clean_desc_owned.is_empty()
            || clean_desc_owned == summary
        {
            Value::Null
        } else {
            Value::String(clean_desc_owned)
        };
        // CLI + MCP invocation forms rendered as Scalar code-sample tabs on
        // the same operation page. Surfaces the 1:1 parity guarantee: every
        // `#[orca_tool]` is callable identically over REST, CLI, and MCP.
        let cli_form = format!("orca {}", entry.name.replace('.', " "));
        let mcp_form = entry.name.replace('.', "_");
        let mcp_sample = serde_json::to_string_pretty(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": mcp_form, "arguments": {} }
        }))
        .unwrap_or_default();
        let path_item = json!({
            "post": {
                "operationId": operation_id_for(entry.name),
                "summary": summary,
                "description": description_field,
                "tags": [domain],
                "x-codeSamples": [
                    { "lang": "shell", "label": "CLI",  "source": format!("{cli_form} '<args-json>'") },
                    { "lang": "json",  "label": "MCP",  "source": mcp_sample },
                ],
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
        wrap_ref_siblings(v);
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

    // Note: `x-tagGroups` is deliberately NOT emitted. With root-domain
    // tagging above (`auth.session` → `auth`), every sub-resource already
    // lands in its parent's tag group naturally; an `x-tagGroups` parent
    // named `auth` would collide with the `auth` tag and render twice in
    // Scalar's left nav.

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

/// Inject the live, plugin-driven managed-unit surface into the spec: one
/// `POST /api/v1/<kind>.<verb|action>` per [`crate::unit_surface::UnitOp`], with
/// the op's typed input/output schemas. `unit` is internal — paths and tags key
/// on the kind (`vm`, `lxc`, `container`). Unlike [`inject_tool_paths`] (which
/// walks the static `inventory` slice), this reads the runtime provider catalog,
/// so the generated spec always reflects currently-loaded plugins. Call it after
/// `inject_tool_paths` so both sets of paths and hoisted defs coexist.
pub fn inject_unit_paths(spec: &mut Value) {
    let ops = crate::unit_surface::unit_ops();
    if ops.is_empty() {
        return;
    }
    let Some(obj) = spec.as_object_mut() else {
        return;
    };

    let mut new_paths: Map<String, Value> = Map::new();
    let mut hoisted_defs: Map<String, Value> = Map::new();
    let mut kinds_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for op in ops {
        let path = format!("/api/v1/{}", op.name);
        let kind = op.kind.clone();
        let mut args_schema = op.input_schema;
        let mut output_schema = op.output_schema;
        kinds_seen.insert(kind.clone());

        hoist_defs(&mut args_schema, &mut hoisted_defs);
        hoist_defs(&mut output_schema, &mut hoisted_defs);
        rewrite_refs(&mut args_schema);
        rewrite_refs(&mut output_schema);
        wrap_ref_siblings(&mut args_schema);
        wrap_ref_siblings(&mut output_schema);
        strip_meta(&mut args_schema);
        strip_meta(&mut output_schema);

        let cli_form = format!("orca {}", op.name.replace('.', " "));
        let mcp_form = op.name.replace('.', "_");
        let mcp_sample = serde_json::to_string_pretty(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": mcp_form, "arguments": {} }
        }))
        .unwrap_or_default();
        let path_item = json!({
            "post": {
                "operationId": operation_id_for(&op.name),
                "summary": op.name,
                "description": op.description,
                "tags": [kind.clone()],
                "x-codeSamples": [
                    { "lang": "shell", "label": "CLI",  "source": format!("{cli_form} --json '<args>'") },
                    { "lang": "json",  "label": "MCP",  "source": mcp_sample },
                ],
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": { "schema": args_schema }
                    }
                },
                "responses": {
                    "200": {
                        "description": "Unit result",
                        "content": {
                            "application/json": { "schema": output_schema }
                        }
                    },
                    "404": tool_error_response("Unknown unit op"),
                    "500": tool_error_response("Unit op failed"),
                }
            }
        });
        new_paths.insert(path, path_item);
    }

    for v in hoisted_defs.values_mut() {
        rewrite_refs(v);
        wrap_ref_siblings(v);
        strip_meta(v);
    }

    let paths = obj
        .entry("paths".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(paths_obj) = paths.as_object_mut() {
        for (k, v) in new_paths {
            paths_obj.insert(k, v);
        }
    }

    let components = obj
        .entry("components".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(components_obj) = components.as_object_mut() {
        let schemas = components_obj
            .entry("schemas".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(schemas_obj) = schemas.as_object_mut() {
            for (k, v) in hoisted_defs {
                schemas_obj.entry(k).or_insert(v);
            }
        }
    }

    if !kinds_seen.is_empty() {
        let tags = obj
            .entry("tags".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(arr) = tags.as_array_mut() {
            for kind in &kinds_seen {
                let exists = arr
                    .iter()
                    .any(|t| t.get("name").and_then(|n| n.as_str()) == Some(kind.as_str()));
                if !exists {
                    arr.push(json!({
                        "name": kind,
                        "description": format!("Managed {kind} units (live, plugin-driven)")
                    }));
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

/// Rewrite `{ $ref, ...other-keys }` siblings into `{ allOf: [{$ref}, {...other-keys}] }`.
///
/// Schemars 1.x emits this sibling shape for internally-tagged enum variants
/// (e.g. `PodMember` via `#[serde(tag = "state")]`): the variant gets a `$ref`
/// to the inner struct PLUS inline `properties` / `required` constraining the
/// discriminator. JSON Schema 2020-12 permits sibling keys with `$ref`, but
/// OpenAPI 3.1 tooling like hey-api drops them — collapsing the discriminator
/// and breaking type-narrowing on the consumer (the `state` field disappears
/// from `PodMember` on the TS side, forcing local casts at every callsite).
///
/// The `allOf` rewrite is semantically equivalent and survives every consumer
/// we've seen.
fn wrap_ref_siblings(v: &mut Value) {
    match v {
        Value::Object(map) => {
            if map.contains_key("$ref") && map.len() > 1 {
                let mut ref_only = Map::new();
                let mut rest = Map::new();
                for (k, val) in std::mem::take(map) {
                    if k == "$ref" {
                        ref_only.insert(k, val);
                    } else {
                        rest.insert(k, val);
                    }
                }
                let mut all_of = vec![Value::Object(ref_only)];
                if !rest.is_empty() {
                    all_of.push(Value::Object(rest));
                }
                map.insert("allOf".to_string(), Value::Array(all_of));
            }
            for child in map.values_mut() {
                wrap_ref_siblings(child);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                wrap_ref_siblings(child);
            }
        }
        _ => {}
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
    fn wrap_ref_siblings_lifts_discriminator_into_all_of() {
        // Schemars emits this shape for `#[serde(tag = "state")]` variants —
        // `$ref` to the inner struct + sibling `properties`/`required` for
        // the tag. hey-api drops the siblings; the allOf form survives.
        let mut v = json!({
            "oneOf": [
                {
                    "$ref": "#/components/schemas/PodPeerDto",
                    "properties": { "state": { "type": "string", "const": "joined" } },
                    "required": ["state"]
                }
            ]
        });
        wrap_ref_siblings(&mut v);
        let variant = &v["oneOf"][0];
        assert!(variant.get("$ref").is_none());
        assert!(variant.get("properties").is_none());
        let all_of = variant["allOf"].as_array().expect("allOf");
        assert_eq!(all_of.len(), 2);
        assert_eq!(all_of[0]["$ref"], "#/components/schemas/PodPeerDto");
        assert_eq!(all_of[1]["properties"]["state"]["const"], "joined");
        assert_eq!(all_of[1]["required"][0], "state");
    }

    #[test]
    fn wrap_ref_siblings_leaves_lone_refs_alone() {
        let mut v = json!({ "$ref": "#/components/schemas/Foo" });
        wrap_ref_siblings(&mut v);
        assert_eq!(v["$ref"], "#/components/schemas/Foo");
        assert!(v.get("allOf").is_none());
    }

    #[test]
    fn hoist_defs_moves_and_clears() {
        let mut schema = json!({ "type": "object", "$defs": { "Foo": { "type": "string" } } });
        let mut out = Map::new();
        hoist_defs(&mut schema, &mut out);
        assert!(schema.get("$defs").is_none());
        assert_eq!(out.get("Foo").unwrap(), &json!({ "type": "string" }));
    }

    #[test]
    fn inject_unit_paths_emits_typed_paths_for_live_providers() {
        use contract::BoxFuture;
        use contract::unit::{
            self, ActionDecl, KindDeclaration, UnitDescriptor, UnitProvider, VerbArgs, VerbDecl,
            VerbOutcome,
        };
        use std::sync::Arc;

        struct P;
        impl UnitProvider for P {
            fn name(&self) -> &str {
                "openapi-unit-test"
            }
            fn declarations(&self) -> Vec<KindDeclaration> {
                vec![KindDeclaration {
                    kind: "gizmo_oa".into(),
                    backup_spec: None,
                    verbs: vec![
                        VerbDecl::list(),
                        VerbDecl {
                            verb: contract::unit::Verb::Update,
                            query_schema: None,
                            actions: vec![ActionDecl {
                                action: "tune".into(),
                                payload_schema: None,
                                response_schema: None,
                            }],
                        },
                    ],
                }]
            }
            fn units(&self) -> BoxFuture<'_, anyhow::Result<Vec<UnitDescriptor>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn invoke(&self, _args: VerbArgs) -> BoxFuture<'_, anyhow::Result<VerbOutcome>> {
                Box::pin(async { Ok(VerbOutcome::Action(Default::default())) })
            }
        }

        unit::register_provider(Arc::new(P));

        let mut spec = json!({ "openapi": "3.1.0", "paths": {} });
        inject_unit_paths(&mut spec);

        let paths = spec["paths"].as_object().unwrap();
        assert!(paths.contains_key("/api/v1/gizmo_oa.list"));
        let tune = &paths["/api/v1/gizmo_oa.tune"]["post"];
        assert_eq!(tune["tags"][0], "gizmo_oa");
        // update op's request body requires an id (a UnitId)
        let schema = &tune["requestBody"]["content"]["application/json"]["schema"];
        assert!(schema["properties"]["id"].is_object());

        assert!(unit::deregister_provider("openapi-unit-test"));
    }
}
