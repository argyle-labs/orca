//! Data-driven endpoint executor — the runtime half of the "generate the whole
//! spec as DATA, share one executor" model.
//!
//! An OpenAPI (or any REST) plugin used to compile one `#[orca_tool]` wrapper
//! fn per operation plus a `JsonSchema`-anchored Rust type per request/response
//! shape. For a large surface (Proxmox VE: 646 operations, ~1350 types) that is
//! ~24 MB of monomorphized serialize/deserialize/schema code — none of which is
//! needed at runtime, since every payload already crosses the FFI/subprocess
//! boundary as `serde_json::Value`.
//!
//! This module replaces all of that with **data**: the plugin's `build.rs`
//! emits a table of [`EndpointDescriptor`] (method + path template + params +
//! embedded JSON-Schema *text*), and this one generic executor performs any of
//! them. The same information as data is ~26× smaller embedded (~245× gzipped)
//! than compiled, and a single `serde_json::Value` codec replaces the ~1350
//! monomorphized ser/de pairs. The trade-off is runtime typing: correctness is
//! enforced by validating **every request and every response** against the
//! embedded schema ([`validate`]) rather than by the Rust type system.
//!
//! Dependency-light on purpose: the validator is hand-rolled (no `jsonschema`
//! crate), transport is [`crate::capsink::http_request`] (no reqwest/rustls).
//! A descriptor-driven plugin links neither progenitor nor a compiled type per
//! operation — it is the thinnest an API-client plugin can be.
#![allow(clippy::disallowed_types)] // Value is the runtime-typed protocol here.

use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

use crate::abi::HttpRequest;

// ── Data model ───────────────────────────────────────────────────────────────

/// Where a parameter rides on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamLoc {
    /// Substituted into the path template (`/nodes/{node}` ← `node`).
    Path,
    /// Appended to the query string (`?full=1`).
    Query,
    /// A member of the JSON request body object.
    Body,
    /// Sent as a request header.
    Header,
}

/// One operation parameter: its wire name, where it rides, and whether the
/// operation requires it. The value is read from the caller's args object by
/// `name`; a missing required param is a validation error before any request.
#[derive(Clone, Copy, Debug)]
pub struct ParamSpec {
    pub name: &'static str,
    pub loc: ParamLoc,
    pub required: bool,
}

/// A single REST operation, as data. Everything the shared [`execute`] needs to
/// perform the call, plus the metadata orca's tool surface advertises. The two
/// `*_schema` fields are raw JSON-Schema **text** (compiled as `&'static str`
/// rodata, not schemars-generated code) and may `$ref` into the table's shared
/// [`DescriptorTable::defs`].
#[derive(Clone, Copy, Debug)]
pub struct EndpointDescriptor {
    /// Full orca tool name, e.g. `"proxmox.get_version"`.
    pub name: &'static str,
    pub description: &'static str,
    /// Uppercase HTTP method.
    pub method: &'static str,
    /// Path template with `{param}` placeholders, e.g. `/nodes/{node}/status`.
    pub path_template: &'static str,
    pub params: &'static [ParamSpec],
    /// JSON-Schema text for the args object (an `object` schema whose properties
    /// are the params plus the always-present `endpoint`).
    pub input_schema: &'static str,
    /// JSON-Schema text for the (envelope-unwrapped) response body.
    pub output_schema: &'static str,
    pub remote_ok: bool,
    pub required_role: &'static str,
    pub data_mutation: bool,
}

/// A plugin's whole surface as data: the descriptor table plus the shared
/// component-schema pool the per-operation schemas `$ref` into. `defs` is the
/// spec's `#/components/schemas` (or `#/$defs`) object as raw JSON text, parsed
/// once and cached in [`Self::defs_value`].
pub struct DescriptorTable {
    pub descriptors: &'static [EndpointDescriptor],
    /// Shared definitions blob (`{"Foo": {…}, …}`) as JSON text. Empty string
    /// or `"{}"` when the surface inlines all schemas.
    pub defs: &'static str,
    #[doc(hidden)]
    pub defs_cache: OnceLock<Value>,
}

impl DescriptorTable {
    /// Construct a table. Use in a `static`:
    /// `static TABLE: DescriptorTable = DescriptorTable::new(&DESCRIPTORS, DEFS);`
    pub const fn new(descriptors: &'static [EndpointDescriptor], defs: &'static str) -> Self {
        Self {
            descriptors,
            defs,
            defs_cache: OnceLock::new(),
        }
    }

    /// The parsed shared definitions object, cached after first parse. A blank
    /// or malformed `defs` yields an empty object (schemas with no `$ref` then
    /// still validate; a `$ref` into a missing pool surfaces as a ref error).
    pub fn defs_value(&self) -> &Value {
        self.defs_cache.get_or_init(|| {
            if self.defs.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(self.defs)
                    .unwrap_or_else(|_| Value::Object(Default::default()))
            }
        })
    }

    /// Find a descriptor by full tool name.
    pub fn find(&self, name: &str) -> Option<&EndpointDescriptor> {
        self.descriptors.iter().find(|d| d.name == name)
    }

    /// The tool manifest (`Vec<ToolDef>`) JSON for this table — the `__manifest`
    /// payload for a descriptor-driven plugin. Each tool's `input_schema` is the
    /// parsed embedded schema; a schema that fails to parse degrades to an open
    /// object so one bad operation never fails the whole `tools/list`.
    pub fn manifest_json(&self) -> String {
        let defs = self.parsed_defs_for_manifest();
        let tools: Vec<Value> = self
            .descriptors
            .iter()
            .map(|d| {
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                    "input_schema": parse_schema_or_open(d.input_schema, &defs),
                })
            })
            .collect();
        serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_string())
    }

    /// Inline `#/$defs` so MCP clients that don't resolve external `$ref` still
    /// get a self-contained input schema. Only attached when the schema uses
    /// `$ref`, to keep the common (ref-free) manifest compact.
    fn parsed_defs_for_manifest(&self) -> Value {
        self.defs_value().clone()
    }
}

fn parse_schema_or_open(text: &str, defs: &Value) -> Value {
    let mut schema: Value = serde_json::from_str(text)
        .unwrap_or_else(|_| serde_json::json!({ "type": "object", "additionalProperties": true }));
    // If the schema references shared defs, embed them as `$defs` so the manifest
    // consumer can resolve them without the pool.
    if schema.to_string().contains("$ref")
        && let Value::Object(map) = &mut schema
        && let Value::Object(defs_map) = defs
        && !defs_map.is_empty()
    {
        map.entry("$defs")
            .or_insert_with(|| Value::Object(defs_map.clone()));
    }
    schema
}

// ── Endpoint resolution ──────────────────────────────────────────────────────

/// A resolved connection target for one call: base URL, default headers (auth),
/// and TLS posture. The plugin produces this from the `endpoint` arg — the only
/// plugin-specific glue the executor needs (≈20 lines vs. 646 fns).
#[derive(Clone, Debug)]
pub struct ResolvedEndpoint {
    /// Absolute base URL with no trailing slash, e.g. `https://pve:8006/api2/json`.
    pub base_url: String,
    /// Headers applied to every request (typically the auth header).
    pub headers: Vec<(String, String)>,
    /// Skip TLS verification (self-signed homelab certs).
    pub insecure: bool,
}

/// Turns the caller's `endpoint` arg into a [`ResolvedEndpoint`]. Implemented by
/// the plugin (usually a lookup of the stored endpoint row → base URL + token).
pub trait EndpointResolver {
    fn resolve(&self, endpoint: &str) -> Result<ResolvedEndpoint>;
}

/// Optional response-envelope peeler (Proxmox's `{"data": …}`). Returns the
/// inner value, or `None` to pass the body through unchanged.
pub type Unwrapper = fn(Value) -> Option<Value>;

// ── Executor ─────────────────────────────────────────────────────────────────

/// Perform `desc` with `args`, validating both request and response against the
/// embedded schemas. This is the whole runtime for a descriptor-driven plugin.
///
/// Steps: validate args → resolve endpoint → fill path/query/body → HTTP via the
/// capability sink → status check → parse+unwrap body → validate response.
pub fn execute(
    desc: &EndpointDescriptor,
    table: &DescriptorTable,
    args: Value,
    resolver: &dyn EndpointResolver,
    unwrap: Option<Unwrapper>,
) -> Result<Value> {
    let defs = table.defs_value();

    // 1. Validate the request args against the operation's input schema.
    let input_schema: Value = serde_json::from_str(desc.input_schema)
        .map_err(|e| anyhow!("{}: malformed input schema: {e}", desc.name))?;
    if let Err(errs) = validate(&args, &input_schema, defs) {
        bail!("{}: invalid request: {}", desc.name, errs.join("; "));
    }

    let obj = args
        .as_object()
        .ok_or_else(|| anyhow!("{}: args must be a JSON object", desc.name))?;

    // 2. Resolve the connection target from the `endpoint` arg.
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{}: missing `endpoint`", desc.name))?;
    let target = resolver.resolve(endpoint)?;

    // 3. Path template substitution + query + body assembly.
    let mut path = desc.path_template.to_string();
    let mut query: Vec<(String, String)> = Vec::new();
    let mut body = serde_json::Map::new();
    for p in desc.params {
        let Some(v) = obj.get(p.name) else { continue };
        if v.is_null() {
            continue;
        }
        match p.loc {
            ParamLoc::Path => {
                let raw = scalar_to_string(v);
                let placeholder = format!("{{{}}}", p.name);
                path = path.replace(&placeholder, &crate::url::encode(&raw));
            }
            ParamLoc::Query => push_query(&mut query, p.name, v),
            ParamLoc::Body => {
                body.insert(p.name.to_string(), v.clone());
            }
            ParamLoc::Header => {}
        }
    }

    let mut url = crate::url::join(&target.base_url, &path);
    if !query.is_empty() {
        let qs = query
            .iter()
            .map(|(k, v)| format!("{}={}", crate::url::encode(k), crate::url::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        url.push('?');
        url.push_str(&qs);
    }

    // 4. Header params + resolved auth headers.
    let mut headers = target.headers.clone();
    for p in desc.params {
        if p.loc == ParamLoc::Header
            && let Some(v) = obj.get(p.name).filter(|v| !v.is_null())
        {
            headers.push((p.name.to_string(), scalar_to_string(v)));
        }
    }
    let body_bytes = if body.is_empty() {
        Vec::new()
    } else {
        headers.push(("content-type".into(), "application/json".into()));
        serde_json::to_vec(&Value::Object(body))?
    };

    // 5. Perform the request over orca's HTTP capability.
    let req = HttpRequest {
        method: desc.method.to_ascii_uppercase(),
        url,
        headers,
        body: body_bytes,
        timeout_ms: None,
        insecure: target.insecure,
    };
    let resp = crate::capsink::http_request(&req)?;

    // 6. Status semantics: surface non-2xx with the upstream body for context.
    if !(200..300).contains(&resp.status) {
        let snippet = String::from_utf8_lossy(&resp.body);
        let snippet = snippet.chars().take(512).collect::<String>();
        bail!("{}: HTTP {} — {}", desc.name, resp.status, snippet);
    }

    // 7. Parse + unwrap the response body. An empty body is `null`.
    let parsed: Value = if resp.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&resp.body)
            .map_err(|e| anyhow!("{}: response is not JSON: {e}", desc.name))?
    };
    let out = match unwrap {
        Some(f) => f(parsed.clone()).unwrap_or(parsed),
        None => parsed,
    };

    // 8. Validate the response against the operation's output schema.
    let output_schema: Value = serde_json::from_str(desc.output_schema)
        .map_err(|e| anyhow!("{}: malformed output schema: {e}", desc.name))?;
    if let Err(errs) = validate(&out, &output_schema, defs) {
        bail!(
            "{}: upstream response failed schema validation: {}",
            desc.name,
            errs.join("; ")
        );
    }
    Ok(out)
}

/// Render a scalar JSON value as its wire string (path/query/header form).
/// Objects/arrays are JSON-encoded (rare for path/query, but well-defined).
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Append a query param. Arrays expand to repeated keys (`k=a&k=b`), matching
/// the common OpenAPI `explode` default.
fn push_query(out: &mut Vec<(String, String)>, key: &str, v: &Value) {
    match v {
        Value::Array(items) => {
            for it in items {
                out.push((key.to_string(), scalar_to_string(it)));
            }
        }
        other => out.push((key.to_string(), scalar_to_string(other))),
    }
}

// ── Dependency-free JSON-Schema validator ────────────────────────────────────

/// Validate `value` against `schema`, resolving `$ref` against `defs`
/// (`#/components/schemas/*` and `#/$defs/*`). Returns every violation found,
/// or `Ok(())`. Covers the OpenAPI-emitted subset: `type` (incl. unions and
/// `nullable`), `required`, `properties`, `additionalProperties`, `items`,
/// `enum`, `const`, and the `allOf`/`anyOf`/`oneOf` combinators. Unknown
/// keywords are ignored (lenient by construction), so undocumented extra
/// response fields never fail a call unless `additionalProperties: false` is
/// explicitly set.
pub fn validate(
    value: &Value,
    schema: &Value,
    defs: &Value,
) -> std::result::Result<(), Vec<String>> {
    let mut errs = Vec::new();
    check(value, schema, defs, "$", &mut errs, 0);
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

const MAX_DEPTH: usize = 128;

fn check(
    value: &Value,
    schema: &Value,
    defs: &Value,
    path: &str,
    errs: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return; // recursion guard for cyclic $ref
    }
    // `true`/`{}` accept anything; `false` rejects everything.
    let obj = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errs.push(format!("{path}: schema `false` rejects all values"));
            return;
        }
        Value::Object(m) => m,
        _ => return,
    };

    // $ref indirection.
    if let Some(Value::String(r)) = obj.get("$ref") {
        match resolve_ref(r, defs) {
            Some(target) => check(value, target, defs, path, errs, depth + 1),
            None => errs.push(format!("{path}: unresolved $ref `{r}`")),
        }
        return;
    }

    // Combinators.
    if let Some(Value::Array(all)) = obj.get("allOf") {
        for sub in all {
            check(value, sub, defs, path, errs, depth + 1);
        }
    }
    if let Some(Value::Array(any)) = obj.get("anyOf") {
        let ok = any
            .iter()
            .any(|sub| validate_sub(value, sub, defs, depth + 1));
        if !ok {
            errs.push(format!("{path}: matched none of anyOf"));
        }
    }
    if let Some(Value::Array(one)) = obj.get("oneOf") {
        let matches = one
            .iter()
            .filter(|sub| validate_sub(value, sub, defs, depth + 1))
            .count();
        if matches != 1 {
            errs.push(format!(
                "{path}: matched {matches} of oneOf (want exactly 1)"
            ));
        }
    }

    // enum / const.
    if let Some(Value::Array(allowed)) = obj.get("enum")
        && !allowed.iter().any(|a| a == value)
    {
        errs.push(format!("{path}: value not in enum"));
    }
    if let Some(c) = obj.get("const")
        && c != value
    {
        errs.push(format!("{path}: value != const"));
    }

    // type (+ nullable).
    let nullable = obj
        .get("nullable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if value.is_null() && nullable {
        return;
    }
    if let Some(t) = obj.get("type")
        && !type_matches(value, t)
    {
        errs.push(format!("{path}: expected type {t}, got {}", kind_of(value)));
        return; // a type mismatch makes deeper checks noise
    }

    match value {
        Value::Object(vmap) => {
            if let Some(Value::Array(req)) = obj.get("required") {
                for r in req.iter().filter_map(Value::as_str) {
                    if !vmap.contains_key(r) {
                        errs.push(format!("{path}: missing required `{r}`"));
                    }
                }
            }
            let props = obj.get("properties").and_then(Value::as_object);
            if let Some(props) = props {
                for (k, sub) in props {
                    if let Some(child) = vmap.get(k) {
                        check(child, sub, defs, &format!("{path}.{k}"), errs, depth + 1);
                    }
                }
            }
            // additionalProperties: false rejects undeclared keys.
            if obj.get("additionalProperties") == Some(&Value::Bool(false))
                && let Some(props) = props
            {
                for k in vmap.keys() {
                    if !props.contains_key(k) {
                        errs.push(format!("{path}: unexpected property `{k}`"));
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for (i, it) in items.iter().enumerate() {
                    check(
                        it,
                        item_schema,
                        defs,
                        &format!("{path}[{i}]"),
                        errs,
                        depth + 1,
                    );
                }
            }
        }
        _ => {}
    }
}

/// True if `value` validates against `schema` with no errors — used for the
/// anyOf/oneOf branch counting.
fn validate_sub(value: &Value, schema: &Value, defs: &Value, depth: usize) -> bool {
    let mut e = Vec::new();
    check(value, schema, defs, "$", &mut e, depth);
    e.is_empty()
}

/// Resolve a local `$ref` (`#/components/schemas/X`, `#/$defs/X`, or `#/X`)
/// against the shared `defs` pool. `defs` is expected to be the *contents* of
/// `components/schemas` (or `$defs`), so the leaf name is looked up directly;
/// a fully-qualified pointer falls back to walking `defs` by its tail segment.
fn resolve_ref<'a>(r: &str, defs: &'a Value) -> Option<&'a Value> {
    let leaf = r.rsplit('/').next()?;
    defs.get(leaf)
}

fn type_matches(value: &Value, t: &Value) -> bool {
    match t {
        Value::String(s) => single_type_matches(value, s),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .any(|s| single_type_matches(value, s)),
        _ => true,
    }
}

fn single_type_matches(value: &Value, t: &str) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        // JSON has no integer type; accept any number with an integral value.
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "number" => value.is_number(),
        _ => true, // unknown type keyword: don't reject
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_defs() -> Value {
        json!({})
    }

    #[test]
    fn accepts_matching_object() {
        let schema = json!({
            "type": "object",
            "required": ["node"],
            "properties": { "node": { "type": "string" }, "full": { "type": "boolean" } }
        });
        let v = json!({ "node": "pve1", "full": true });
        assert!(validate(&v, &schema, &no_defs()).is_ok());
    }

    #[test]
    fn flags_missing_required_and_wrong_type() {
        let schema = json!({
            "type": "object",
            "required": ["node"],
            "properties": { "node": { "type": "string" } }
        });
        let v = json!({ "node": 5 });
        let errs = validate(&v, &schema, &no_defs()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("node")), "{errs:?}");
    }

    #[test]
    fn integer_accepts_integral_number_rejects_fraction() {
        let s = json!({ "type": "integer" });
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        assert!(validate(&json!(3.5), &s, &no_defs()).is_err());
    }

    #[test]
    fn nullable_allows_null() {
        let s = json!({ "type": "string", "nullable": true });
        assert!(validate(&Value::Null, &s, &no_defs()).is_ok());
    }

    #[test]
    fn resolves_ref_into_defs() {
        let defs = json!({ "Node": { "type": "object", "required": ["id"], "properties": { "id": { "type": "string" } } } });
        let schema = json!({ "$ref": "#/components/schemas/Node" });
        assert!(validate(&json!({ "id": "x" }), &schema, &defs).is_ok());
        assert!(validate(&json!({}), &schema, &defs).is_err());
    }

    #[test]
    fn additional_properties_false_rejects_extras() {
        let s = json!({ "type": "object", "properties": { "a": { "type": "string" } }, "additionalProperties": false });
        assert!(validate(&json!({ "a": "x" }), &s, &no_defs()).is_ok());
        assert!(validate(&json!({ "a": "x", "b": 1 }), &s, &no_defs()).is_err());
    }

    #[test]
    fn lenient_on_unknown_extra_fields_by_default() {
        // No additionalProperties → extra response fields are accepted (real
        // appliances return undocumented keys).
        let s = json!({ "type": "object", "properties": { "a": { "type": "string" } } });
        assert!(validate(&json!({ "a": "x", "extra": 99 }), &s, &no_defs()).is_ok());
    }

    #[test]
    fn any_of_matches_one_branch() {
        let s = json!({ "anyOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        assert!(validate(&json!(true), &s, &no_defs()).is_err());
    }

    #[test]
    fn manifest_lists_every_descriptor() {
        static DS: &[EndpointDescriptor] = &[EndpointDescriptor {
            name: "demo.get_thing",
            description: "get a thing",
            method: "GET",
            path_template: "/thing/{id}",
            params: &[ParamSpec {
                name: "id",
                loc: ParamLoc::Path,
                required: true,
            }],
            input_schema: r#"{"type":"object","required":["endpoint","id"],"properties":{"endpoint":{"type":"string"},"id":{"type":"string"}}}"#,
            output_schema: r#"{"type":"object"}"#,
            remote_ok: false,
            required_role: "read",
            data_mutation: false,
        }];
        static TABLE: DescriptorTable = DescriptorTable::new(DS, "{}");
        let manifest: Vec<Value> = serde_json::from_str(&TABLE.manifest_json()).unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0]["name"], "demo.get_thing");
        assert_eq!(
            manifest[0]["input_schema"]["properties"]["id"]["type"],
            "string"
        );
    }

    static DS_FIND: &[EndpointDescriptor] = &[EndpointDescriptor {
        name: "demo.get_thing",
        description: "d",
        method: "GET",
        path_template: "/t",
        params: &[],
        input_schema: r#"{"type":"object"}"#,
        output_schema: r#"{"type":"object"}"#,
        remote_ok: false,
        required_role: "read",
        data_mutation: false,
    }];

    #[test]
    fn find_by_name() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        assert!(T.find("demo.get_thing").is_some());
        assert!(T.find("nope").is_none());
    }

    #[test]
    fn defs_value_blank_and_malformed_yield_empty_object() {
        static BLANK: DescriptorTable = DescriptorTable::new(DS_FIND, "   ");
        assert_eq!(BLANK.defs_value(), &json!({}));
        static BAD: DescriptorTable = DescriptorTable::new(DS_FIND, "{not json");
        assert_eq!(BAD.defs_value(), &json!({}));
        static GOOD: DescriptorTable = DescriptorTable::new(DS_FIND, r#"{"A":{"type":"string"}}"#);
        assert_eq!(GOOD.defs_value(), &json!({ "A": { "type": "string" } }));
    }

    #[test]
    fn parse_schema_or_open_embeds_defs_on_ref() {
        let defs = json!({ "Node": { "type": "object" } });
        let with_ref = parse_schema_or_open(r##"{"$ref":"#/components/schemas/Node"}"##, &defs);
        assert_eq!(with_ref["$defs"]["Node"]["type"], "object");
        // No $ref ⇒ no $defs injected.
        let no_ref = parse_schema_or_open(r#"{"type":"string"}"#, &defs);
        assert!(no_ref.get("$defs").is_none());
        // Malformed ⇒ open object fallback.
        let bad = parse_schema_or_open("{oops", &defs);
        assert_eq!(bad["type"], "object");
        assert_eq!(bad["additionalProperties"], true);
    }

    #[test]
    fn manifest_embeds_defs_for_ref_schema() {
        static DS: &[EndpointDescriptor] = &[EndpointDescriptor {
            name: "demo.node",
            description: "d",
            method: "GET",
            path_template: "/n",
            params: &[],
            input_schema: r##"{"$ref":"#/components/schemas/Node"}"##,
            output_schema: r#"{"type":"object"}"#,
            remote_ok: false,
            required_role: "read",
            data_mutation: false,
        }];
        static T: DescriptorTable = DescriptorTable::new(
            DS,
            r#"{"Node":{"type":"object","properties":{"id":{"type":"string"}}}}"#,
        );
        let m: Vec<Value> = serde_json::from_str(&T.manifest_json()).unwrap();
        assert_eq!(m[0]["input_schema"]["$defs"]["Node"]["type"], "object");
    }

    #[test]
    fn scalar_to_string_covers_all_kinds() {
        assert_eq!(scalar_to_string(&json!("x")), "x");
        assert_eq!(scalar_to_string(&json!(true)), "true");
        assert_eq!(scalar_to_string(&json!(5)), "5");
        assert_eq!(scalar_to_string(&Value::Null), "");
        assert_eq!(scalar_to_string(&json!([1, 2])), "[1,2]");
    }

    #[test]
    fn push_query_expands_arrays_and_scalars() {
        let mut out = Vec::new();
        push_query(&mut out, "k", &json!("a"));
        push_query(&mut out, "tags", &json!(["x", "y"]));
        assert_eq!(
            out,
            vec![
                ("k".to_string(), "a".to_string()),
                ("tags".to_string(), "x".to_string()),
                ("tags".to_string(), "y".to_string()),
            ]
        );
    }

    #[test]
    fn single_type_matches_all_keywords() {
        assert!(single_type_matches(&json!({}), "object"));
        assert!(single_type_matches(&json!([]), "array"));
        assert!(single_type_matches(&json!("s"), "string"));
        assert!(single_type_matches(&json!(true), "boolean"));
        assert!(single_type_matches(&Value::Null, "null"));
        assert!(single_type_matches(&json!(4), "integer"));
        assert!(single_type_matches(&json!(4.0), "integer"));
        assert!(!single_type_matches(&json!(4.5), "integer"));
        assert!(single_type_matches(&json!(4.5), "number"));
        assert!(single_type_matches(&json!("x"), "unknownkw"));
    }

    #[test]
    fn kind_of_names_every_variant() {
        assert_eq!(kind_of(&Value::Null), "null");
        assert_eq!(kind_of(&json!(true)), "boolean");
        assert_eq!(kind_of(&json!(1)), "number");
        assert_eq!(kind_of(&json!("s")), "string");
        assert_eq!(kind_of(&json!([])), "array");
        assert_eq!(kind_of(&json!({})), "object");
    }

    #[test]
    fn type_union_array_matches_any() {
        let s = json!({ "type": ["string", "null"] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        assert!(validate(&Value::Null, &s, &no_defs()).is_ok());
        assert!(validate(&json!(3), &s, &no_defs()).is_err());
    }

    #[test]
    fn bool_schema_true_and_false() {
        assert!(validate(&json!(1), &json!(true), &no_defs()).is_ok());
        assert!(validate(&json!(1), &json!(false), &no_defs()).is_err());
    }

    #[test]
    fn all_of_requires_every_subschema() {
        let s = json!({
            "allOf": [
                { "type": "object", "required": ["a"] },
                { "type": "object", "required": ["b"] }
            ]
        });
        assert!(validate(&json!({ "a": 1, "b": 2 }), &s, &no_defs()).is_ok());
        assert!(validate(&json!({ "a": 1 }), &s, &no_defs()).is_err());
    }

    #[test]
    fn one_of_wants_exactly_one() {
        let s = json!({ "oneOf": [ { "type": "string" }, { "type": "integer" } ] });
        assert!(validate(&json!("x"), &s, &no_defs()).is_ok());
        // A plain integer matches "integer" only.
        assert!(validate(&json!(3), &s, &no_defs()).is_ok());
        // Matches neither ⇒ error.
        assert!(validate(&json!(true), &s, &no_defs()).is_err());
    }

    #[test]
    fn const_must_equal() {
        let s = json!({ "const": "fixed" });
        assert!(validate(&json!("fixed"), &s, &no_defs()).is_ok());
        assert!(validate(&json!("other"), &s, &no_defs()).is_err());
    }

    #[test]
    fn enum_membership() {
        let s = json!({ "enum": ["a", "b"] });
        assert!(validate(&json!("a"), &s, &no_defs()).is_ok());
        assert!(validate(&json!("z"), &s, &no_defs()).is_err());
    }

    #[test]
    fn unresolved_ref_reports_error() {
        let s = json!({ "$ref": "#/components/schemas/Missing" });
        let errs = validate(&json!({}), &s, &no_defs()).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unresolved $ref")),
            "{errs:?}"
        );
    }

    #[test]
    fn array_items_validated_elementwise() {
        let s = json!({ "type": "array", "items": { "type": "integer" } });
        assert!(validate(&json!([1, 2, 3]), &s, &no_defs()).is_ok());
        let errs = validate(&json!([1, "bad"]), &s, &no_defs()).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("[1]")), "{errs:?}");
    }

    #[test]
    fn non_object_non_bool_schema_accepts() {
        // A schema that isn't an object/bool (e.g. a bare string) accepts anything.
        assert!(validate(&json!(1), &json!("weird"), &no_defs()).is_ok());
    }
}
