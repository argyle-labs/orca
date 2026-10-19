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
//! embedded schema ([`utils::json_validate::validate`]) rather than by the Rust type system.
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
                    "role": d.required_role,
                    "data_mutation": d.data_mutation,
                    "local_only": !d.remote_ok,
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
    if let Err(errs) = ::utils::json_validate::validate(&args, &input_schema, defs) {
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
                let raw = ::utils::json_wire::wire_string(v);
                let placeholder = format!("{{{}}}", p.name);
                path = path.replace(&placeholder, &crate::url::encode(&raw));
            }
            ParamLoc::Query => ::utils::json_wire::push_query(&mut query, p.name, v),
            ParamLoc::Body => {
                body.insert(p.name.to_string(), v.clone());
            }
            ParamLoc::Header => {}
        }
    }

    let mut url = crate::url::join(&target.base_url, &path);
    if !query.is_empty() {
        url.push('?');
        url.push_str(&crate::url::query_string(&query));
    }

    // 4. Header params + resolved auth headers.
    let mut headers = target.headers.clone();
    for p in desc.params {
        if p.loc == ParamLoc::Header
            && let Some(v) = obj.get(p.name).filter(|v| !v.is_null())
        {
            headers.push((p.name.to_string(), ::utils::json_wire::wire_string(v)));
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
        max_body_bytes: None,
        body_b64: true,
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
    if let Err(errs) = ::utils::json_validate::validate(&out, &output_schema, defs) {
        bail!(
            "{}: upstream response failed schema validation: {}",
            desc.name,
            errs.join("; ")
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifest_carries_policy_flags() {
        static DESCRIPTORS: [EndpointDescriptor; 2] = [
            EndpointDescriptor {
                name: "fake.update",
                description: "",
                method: "POST",
                path_template: "/update",
                params: &[],
                input_schema: "{}",
                output_schema: "{}",
                remote_ok: false,
                required_role: "admin",
                data_mutation: true,
            },
            EndpointDescriptor {
                name: "fake.status",
                description: "",
                method: "GET",
                path_template: "/status",
                params: &[],
                input_schema: "{}",
                output_schema: "{}",
                remote_ok: true,
                required_role: "read",
                data_mutation: false,
            },
        ];
        let table = DescriptorTable::new(&DESCRIPTORS, "");
        let tools: Value = serde_json::from_str(&table.manifest_json()).unwrap();
        assert_eq!(tools[0]["role"], "admin");
        assert_eq!(tools[0]["data_mutation"], true);
        assert_eq!(tools[0]["local_only"], true);
        assert_eq!(tools[1]["role"], "read");
        assert_eq!(tools[1]["data_mutation"], false);
        assert_eq!(tools[1]["local_only"], false);
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

    // ── execute() error-path coverage ────────────────────────────────────────

    struct OkResolver;
    impl EndpointResolver for OkResolver {
        fn resolve(&self, _endpoint: &str) -> Result<ResolvedEndpoint> {
            Ok(ResolvedEndpoint {
                base_url: "https://host/api".to_string(),
                headers: vec![],
                insecure: false,
            })
        }
    }

    struct ErrResolver;
    impl EndpointResolver for ErrResolver {
        fn resolve(&self, _endpoint: &str) -> Result<ResolvedEndpoint> {
            bail!("no such endpoint")
        }
    }

    fn desc_with(input_schema: &'static str) -> EndpointDescriptor {
        EndpointDescriptor {
            name: "demo.op",
            description: "d",
            method: "GET",
            path_template: "/x",
            params: &[],
            input_schema,
            output_schema: r#"{"type":"object"}"#,
            remote_ok: false,
            required_role: "read",
            data_mutation: false,
        }
    }

    #[test]
    fn execute_rejects_malformed_input_schema() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        let d = desc_with("{ not json");
        let err = execute(&d, &T, json!({}), &OkResolver, None).unwrap_err();
        assert!(err.to_string().contains("malformed input schema"), "{err}");
    }

    #[test]
    fn execute_rejects_invalid_request() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        let d = desc_with(
            r#"{"type":"object","required":["endpoint"],"properties":{"endpoint":{"type":"string"}}}"#,
        );
        let err = execute(&d, &T, json!({}), &OkResolver, None).unwrap_err();
        assert!(err.to_string().contains("invalid request"), "{err}");
    }

    #[test]
    fn execute_rejects_non_object_args() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        // `true` schema accepts a bare scalar, so the as_object guard fires next.
        let d = desc_with("true");
        let err = execute(&d, &T, json!(5), &OkResolver, None).unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn execute_requires_endpoint_arg() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        let d = desc_with("true");
        let err = execute(&d, &T, json!({}), &OkResolver, None).unwrap_err();
        assert!(err.to_string().contains("missing `endpoint`"), "{err}");
    }

    #[test]
    fn execute_propagates_resolver_error() {
        static T: DescriptorTable = DescriptorTable::new(DS_FIND, "{}");
        let d = desc_with("true");
        let err = execute(&d, &T, json!({ "endpoint": "e" }), &ErrResolver, None).unwrap_err();
        assert!(err.to_string().contains("no such endpoint"), "{err}");
    }
}
