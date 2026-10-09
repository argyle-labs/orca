//! Object-safe wrapper around OrcaTool.
//!
//! OrcaTool has associated types + async fn, so `dyn OrcaTool` doesn't work.
//! ErasedTool erases those details so tools can live in a Vec<Box<dyn ErasedTool>>.
//! Output is normalized to `serde_json::Value`: text-returning tools end up as
//! `Value::String`; structured tools serialize directly. Callers that need text
//! (MCP, CLI) call `value_to_text()` to render.
//!
//! `serde_json::Value` is the tool dispatch protocol here — it is the normalized
//! wire representation across the type-erased boundary (ErasedTool). Every
//! concrete tool's strongly-typed Args/Output is serialized to/from Value
//! at the edge. This is the designated opaque layer in the tool surface stack.
#![allow(clippy::disallowed_types)]

use anyhow::Result;
use futures::future::BoxFuture;
use serde_json::Value;
use std::marker::PhantomData;

use contract::{OrcaTool, ToolCtx};

/// A tool's typed args from its JSON. A refusal is the caller's error (`Invalid`,
/// HTTP 400), not a server fault. Its message carries the offending field's path
/// where serde can track one, which it cannot through `#[serde(flatten)]`, and
/// any hint the ctx's [`contract::ArgRefusalHint`] adds.
fn parse_args<T: OrcaTool>(args: Value, ctx: &ToolCtx) -> Result<T::Args> {
    parse_named_args(T::NAME, args).map_err(|e| with_refusal_hint(e, ctx))
}

fn with_refusal_hint(err: anyhow::Error, ctx: &ToolCtx) -> anyhow::Error {
    let Ok(hinter) = ctx.service::<std::sync::Arc<dyn contract::ArgRefusalHint>>() else {
        return err;
    };
    match err.downcast::<contract::OrcaError>() {
        Ok(mut oe) => {
            if let Some(hint) = hinter.hint(ctx, &oe.message) {
                oe.message = format!("{}; {hint}", oe.message);
            }
            oe.into()
        }
        Err(err) => err,
    }
}

/// [`parse_args`] for a surface whose tools are not `OrcaTool`s.
pub(crate) fn parse_named_args<A: serde::de::DeserializeOwned>(
    tool: &str,
    args: Value,
) -> Result<A> {
    serde_path_to_error::deserialize(args).map_err(|e| {
        contract::OrcaError::invalid(format!("invalid args for {tool}: {e}"))
            .with_code("args.invalid")
            .into()
    })
}

/// Asserts `err` is an `Invalid` refusal whose message contains `needle`.
#[cfg(test)]
pub(crate) fn assert_invalid(err: &anyhow::Error, needle: &str) {
    let oe = err
        .downcast_ref::<contract::OrcaError>()
        .unwrap_or_else(|| panic!("not an OrcaError: {err}"));
    assert_eq!(oe.kind, contract::ErrorKind::Invalid, "{oe}");
    assert!(oe.message.contains(needle), "{oe}");
}

/// Object-safe version of OrcaTool. Implemented automatically for any OrcaTool via ToolWrapper.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// Whether this tool may be invoked by a paired system via `mesh/exec`.
    fn remote_ok(&self) -> bool;
    /// Minimum role required to invoke this tool via authenticated surfaces
    /// (REST). Mirrors `OrcaToolDef::REQUIRED_ROLE`. CLI / loopback / MCP-stdio
    /// are not gated here — those run in-process as the daemon owner.
    fn required_role(&self) -> &'static str;
    /// Whether this tool is a data mutation (write against an external managed
    /// system). Mirrors `OrcaToolDef::DATA_MUTATION`. Lets a non-admin identity
    /// holding the `can_mutate` opt-in invoke it despite `required_role` being
    /// `"admin"`; control-plane admin tools leave this false.
    fn data_mutation(&self) -> bool;
    /// Whether this tool APPLIES changes and so requires an explicit `execute`
    /// opt-in. Mirrors `OrcaToolDef::EXECUTE_GATED`. Dry-run is the default:
    /// without the opt-in, [`Self::run_json`] returns an `ExecutionPlan` and
    /// the tool body never runs.
    fn execute_gated(&self) -> bool;
    /// JSON Schema for this tool's Args — used for MCP tools/list, CLI flag generation,
    /// OpenAPI request body, and TS `.d.ts` emission.
    fn input_schema(&self) -> Value;
    /// JSON Schema for this tool's Output — used for OpenAPI response body and
    /// TS `.d.ts` emission.
    fn output_schema(&self) -> Value;
    /// Deserialize args from JSON, run the tool, return output as JSON value.
    fn run_json<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Value>>;
}

/// Zero-sized wrapper that implements ErasedTool for any T: OrcaTool.
pub struct ToolWrapper<T>(pub PhantomData<T>);

// PhantomData<T> is Send+Sync when T: Send+Sync, which OrcaTool requires.
unsafe impl<T: OrcaTool> Send for ToolWrapper<T> {}
unsafe impl<T: OrcaTool> Sync for ToolWrapper<T> {}

impl<T: OrcaTool> ErasedTool for ToolWrapper<T> {
    fn name(&self) -> &'static str {
        T::NAME
    }

    fn description(&self) -> &'static str {
        T::DESCRIPTION
    }

    fn remote_ok(&self) -> bool {
        T::REMOTE_OK
    }

    fn required_role(&self) -> &'static str {
        T::REQUIRED_ROLE
    }

    fn data_mutation(&self) -> bool {
        T::DATA_MUTATION
    }

    fn execute_gated(&self) -> bool {
        T::EXECUTE_GATED
    }

    fn input_schema(&self) -> Value {
        let schema = schema_for::<T::Args>();
        // A gated tool's Args deliberately do not carry `execute` — the gate
        // reads it off the raw args. Advertise it here so OpenAPI, MCP
        // tools/list and CLI flag generation all show the opt-in that callers
        // must pass, instead of it being an undocumented magic field.
        if T::EXECUTE_GATED {
            return with_execute_opt_in(schema);
        }
        schema
    }

    fn output_schema(&self) -> Value {
        schema_for::<T::Output>()
    }

    fn run_json<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            // Dry-run is the DEFAULT for every verb that applies changes. No
            // opt-in ⇒ describe and return; the tool body is never entered, so
            // this cannot half-apply. Enforced here because `run_json` is the
            // one path all surfaces (REST, MCP, CLI, mesh/exec) funnel through.
            let args = if T::EXECUTE_GATED {
                if !execute_opt_in(&args) {
                    let args = without_execute(args);
                    // A plan for inputs the verb would refuse is a false
                    // promise, so they are rejected here exactly as an apply
                    // would reject them.
                    parse_args::<T>(args.clone(), ctx)?;
                    let plan = contract::plan::ExecutionPlan::generic(T::NAME, args.into());
                    return serde_json::to_value(&plan).map_err(|e| {
                        anyhow::anyhow!("failed to serialize plan for {}: {e}", T::NAME)
                    });
                }
                // Opting in is not the same as being allowed. A caller who
                // asks to APPLY must hold the verb's permission, checked here
                // and not only at the surface: the REST middleware enforced
                // this, MCP re-implemented it, and the CLI and mesh/exec paths
                // enforced nothing at all. Three copies and two gaps is how a
                // permission model becomes decorative.
                //
                // Refused loudly, never downgraded to a dry run — a surface
                // that returns a plan where the caller asked to apply reports
                // success for work it did not do.
                if let Some(caller) = ctx.caller()
                    && !crate::tool_roles::authorize(
                        &caller.role,
                        caller.can_mutate,
                        T::REQUIRED_ROLE,
                        T::DATA_MUTATION,
                    )
                {
                    anyhow::bail!(
                        "{} requires role '{}' to execute; caller '{}' has '{}'",
                        T::NAME,
                        T::REQUIRED_ROLE,
                        caller.username,
                        caller.role
                    );
                }
                // Strip the opt-in before typed deserialization: it is the
                // gate's field, not the verb's, and `deny_unknown_fields` args
                // would otherwise reject it.
                without_execute(args)
            } else {
                args
            };
            let parsed = parse_args::<T>(args, ctx)?;
            let out = T::run(parsed, ctx).await?;
            serde_json::to_value(&out)
                .map_err(|e| anyhow::anyhow!("failed to serialize output of {}: {e}", T::NAME))
        })
    }
}

/// True when the caller explicitly opted in to apply changes. Anything other
/// than boolean `true` — absent, `false`, a string, `null` — is NOT consent.
fn execute_opt_in(args: &Value) -> bool {
    args.get(contract::plan::EXECUTE_FIELD)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Remove the gate's own field so it never reaches the verb's typed `Args`.
fn without_execute(mut args: Value) -> Value {
    if let Some(obj) = args.as_object_mut() {
        obj.remove(contract::plan::EXECUTE_FIELD);
    }
    args
}

/// Add the `execute` opt-in to a gated tool's advertised input schema. Leaves
/// an existing `execute` property alone — a verb that already models its own
/// opt-in keeps its documentation and semantics.
fn with_execute_opt_in(mut schema: Value) -> Value {
    let Some(obj) = schema.as_object_mut() else {
        return schema;
    };
    let props = obj
        .entry("properties")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(map) = props.as_object_mut()
        && !map.contains_key(contract::plan::EXECUTE_FIELD)
    {
        map.insert(
            contract::plan::EXECUTE_FIELD.to_string(),
            serde_json::json!({
                "type": "boolean",
                "default": false,
                "description": "Apply the change. Omitted or false returns an ExecutionPlan describing what would happen, and changes nothing.",
            }),
        );
    }
    schema
}

fn schema_for<T: schemars::JsonSchema>() -> Value {
    sanitize_schema(schemars::schema_for!(T).into())
}

/// Strip schemars bookkeeping keys and coerce typeless properties into a
/// shape MCP clients accept. Split from `schema_for` so the non-object-root
/// path is reachable in tests (schemars always emits an object root, so it
/// can't be hit through the generic helper).
fn sanitize_schema(mut v: Value) -> Value {
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    normalize_schema(&mut v);
    v
}

/// Coerce untyped property schemas into a concrete shape.
///
/// `serde_json::Value` fields render as a typeless "any" schema (no `type`
/// key). MCP clients reject input-schema properties they can't resolve to a
/// JSON type, and one bad tool fails the entire `tools/list`. We treat any
/// property schema that lacks a `type` and any other type-discriminating
/// keyword as an open object.
fn normalize_schema(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };

    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for prop in props.values_mut() {
            coerce_untyped(prop);
            normalize_schema(prop);
        }
    }

    for key in ["items", "additionalProperties"] {
        if let Some(child) = obj.get_mut(key) {
            normalize_schema(child);
        }
    }

    for key in ["oneOf", "anyOf", "allOf"] {
        if let Some(Value::Array(variants)) = obj.get_mut(key) {
            for variant in variants {
                normalize_schema(variant);
            }
        }
    }
}

/// Give an open "any" property schema a concrete object type so MCP clients
/// accept it. Leaves any schema that already resolves to a type untouched.
fn coerce_untyped(prop: &mut Value) {
    let has_type = match prop {
        Value::Object(m) => ["type", "$ref", "oneOf", "anyOf", "allOf", "enum", "const"]
            .iter()
            .any(|k| m.contains_key(*k)),
        // `true`/`{}` are valid "any" schemas with no type information.
        Value::Bool(_) => false,
        _ => true,
    };
    if has_type {
        return;
    }

    let mut m = match prop.take() {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    m.insert("type".into(), Value::String("object".into()));
    m.insert("additionalProperties".into(), Value::Bool(true));
    *prop = Value::Object(m);
}

/// Render a JSON value as the plain-text form that MCP/CLI consumers expect.
/// String values pass through; anything else is pretty-printed JSON.
pub fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use contract::{OrcaTool, OrcaToolDef, ToolCtx};
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use std::sync::Arc;

    #[derive(Deserialize, Serialize, JsonSchema)]
    struct Args {
        n: i64,
    }

    #[test]
    fn untyped_value_property_is_coerced_to_open_object() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct OpaqueArgs {
            variables: Option<Value>,
            name: String,
        }

        let schema = schema_for::<OpaqueArgs>();
        let variables = &schema["properties"]["variables"];
        assert_eq!(variables["type"], Value::String("object".into()));
        assert_eq!(variables["additionalProperties"], Value::Bool(true));

        // A concretely-typed property must be left untouched.
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    #[test]
    fn coerce_untyped_open_object_preserves_existing_keys() {
        // Object with no type-discriminating keyword → coerced, but its
        // existing keys (e.g. description) are preserved. Covers the
        // `take() => Object` arm.
        let mut prop = serde_json::json!({ "description": "freeform" });
        coerce_untyped(&mut prop);
        assert_eq!(prop["type"], "object");
        assert_eq!(prop["additionalProperties"], Value::Bool(true));
        assert_eq!(prop["description"], "freeform");
    }

    #[test]
    fn coerce_untyped_handles_bool_any_schema() {
        // A bare `true`/`{}` "any" schema (what an untyped `Value` emits) is
        // coerced into an open object. Covers the `Bool` + `take() => _` arm.
        let mut prop = Value::Bool(true);
        coerce_untyped(&mut prop);
        assert_eq!(prop["type"], "object");
        assert_eq!(prop["additionalProperties"], Value::Bool(true));
    }

    #[test]
    fn coerce_untyped_leaves_typed_and_non_schema_values_alone() {
        // Already-typed object: untouched.
        let mut typed = serde_json::json!({ "type": "string" });
        coerce_untyped(&mut typed);
        assert_eq!(typed, serde_json::json!({ "type": "string" }));

        // Each type-discriminating keyword short-circuits.
        for key in ["$ref", "oneOf", "anyOf", "allOf", "enum", "const"] {
            let mut prop = serde_json::json!({ key: "x" });
            coerce_untyped(&mut prop);
            assert!(
                prop.get("additionalProperties").is_none(),
                "{key} was coerced"
            );
        }

        // A non-object, non-bool value is not a schema we rewrite. Covers
        // the `_ => true` arm.
        let mut scalar = Value::String("not-a-schema".into());
        coerce_untyped(&mut scalar);
        assert_eq!(scalar, Value::String("not-a-schema".into()));
    }

    #[test]
    fn normalize_schema_recurses_into_combinators_and_nested_containers() {
        // Untyped `Value` properties nested inside oneOf/anyOf/allOf, items,
        // and additionalProperties must all be coerced. Covers the
        // combinator-array recursion arm.
        let mut schema = serde_json::json!({
            "type": "object",
            "properties": {
                "list": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "deep": true } }
                },
                "map": {
                    "type": "object",
                    "additionalProperties": { "type": "object", "properties": { "v": true } }
                }
            },
            "oneOf": [ { "type": "object", "properties": { "a": true } } ],
            "anyOf": [ { "type": "object", "properties": { "b": true } } ],
            "allOf": [ { "type": "object", "properties": { "c": true } } ]
        });
        normalize_schema(&mut schema);

        assert_eq!(
            schema["properties"]["list"]["items"]["properties"]["deep"]["type"],
            "object"
        );
        assert_eq!(
            schema["properties"]["map"]["additionalProperties"]["properties"]["v"]["type"],
            "object"
        );
        assert_eq!(schema["oneOf"][0]["properties"]["a"]["type"], "object");
        assert_eq!(schema["anyOf"][0]["properties"]["b"]["type"], "object");
        assert_eq!(schema["allOf"][0]["properties"]["c"]["type"], "object");
    }

    #[test]
    fn normalize_schema_ignores_non_object_nodes() {
        // Early-return path: a non-object node is left untouched.
        let mut node = Value::String("scalar".into());
        normalize_schema(&mut node);
        assert_eq!(node, Value::String("scalar".into()));
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct Out {
        doubled: i64,
    }

    struct DoubleTool;

    impl OrcaToolDef for DoubleTool {
        const NAME: &'static str = "double";
        const DESCRIPTION: &'static str = "doubles n";
        const REMOTE_OK: bool = true;
        const REQUIRED_ROLE: &'static str = "admin";
        type Args = Args;
        type Output = Out;
    }

    #[async_trait]
    impl OrcaTool for DoubleTool {
        async fn run(args: Args, _ctx: &ToolCtx) -> Result<Out> {
            Ok(Out {
                doubled: args.n * 2,
            })
        }
    }

    /// Output whose serializer always errors — exercises the serialize-
    /// failure branch in `run_json`. We hand-roll Serialize to fail and
    /// derive everything else from a unit struct wrapper.
    #[derive(Deserialize, JsonSchema)]
    struct BrokenOut;

    impl Serialize for BrokenOut {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("intentional"))
        }
    }

    struct ErrTool;

    impl OrcaToolDef for ErrTool {
        const NAME: &'static str = "err";
        const DESCRIPTION: &'static str = "always errs";
        type Args = Args;
        type Output = Out;
    }

    #[async_trait]
    impl OrcaTool for ErrTool {
        async fn run(_args: Args, _ctx: &ToolCtx) -> Result<Out> {
            anyhow::bail!("boom")
        }
    }

    struct BrokenSerializeTool;

    impl OrcaToolDef for BrokenSerializeTool {
        const NAME: &'static str = "broken";
        const DESCRIPTION: &'static str = "always fails to serialize";
        type Args = Args;
        type Output = BrokenOut;
    }

    #[async_trait]
    impl OrcaTool for BrokenSerializeTool {
        async fn run(_args: Args, _ctx: &ToolCtx) -> Result<BrokenOut> {
            Ok(BrokenOut)
        }
    }

    fn ctx() -> ToolCtx {
        use contract::config::{Config, Model};
        ToolCtx::new(Arc::new(Config {
            anthropic_api_key: None,
            lmstudio_url: "http://localhost:1234".into(),
            ollama_url: "http://localhost:11434".into(),
            default_model: Model::LMStudio {
                id: String::new(),
                url: String::new(),
            },
            app_dir: PathBuf::from("/tmp"),
            memory_root: PathBuf::from("/tmp"),
            db_path: PathBuf::from("/tmp/test.db"),
            ports: Default::default(),
        }))
    }

    #[test]
    fn name_description_remote_ok_required_role_are_forwarded() {
        let w = ToolWrapper::<DoubleTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        assert_eq!(e.name(), "double");
        assert_eq!(e.description(), "doubles n");
        assert!(e.remote_ok());
        assert_eq!(e.required_role(), "admin");
    }

    #[test]
    fn input_and_output_schemas_drop_schema_and_title_keys() {
        let w = ToolWrapper::<DoubleTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let inp = e.input_schema();
        let out = e.output_schema();
        for v in [&inp, &out] {
            let obj = v.as_object().expect("schema is an object");
            assert!(!obj.contains_key("$schema"));
            assert!(!obj.contains_key("title"));
        }
        // Output schema must mention the field name to prove we routed through
        // the actual Output associated type, not the Args one.
        assert!(out.to_string().contains("doubled"));
    }

    #[tokio::test]
    async fn run_json_happy_path_returns_serialized_output() {
        let w = ToolWrapper::<DoubleTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let v = e
            .run_json(serde_json::json!({"n": 21}), &ctx())
            .await
            .expect("ok");
        assert_eq!(v, serde_json::json!({"doubled": 42}));
    }

    #[tokio::test]
    async fn run_json_invalid_args_returns_named_error() {
        let w = ToolWrapper::<DoubleTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let err = e.run_json(serde_json::json!({}), &ctx()).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid args for double"), "got: {msg}");
    }

    #[test]
    fn every_erased_method_is_exercised_on_all_tool_instantiations() {
        // Each `ToolWrapper<T>` monomorphization gets its own copy of every
        // method's regions. The other tests only call `run_json` on ErrTool /
        // BrokenSerializeTool, leaving their metadata + schema regions (and
        // `schema_for::<BrokenOut>`) uncovered. Exercise every method on every
        // wrapper so no instantiation has dead regions.
        let err = ToolWrapper::<ErrTool>(PhantomData);
        let broken = ToolWrapper::<BrokenSerializeTool>(PhantomData);
        let wrappers: [&dyn ErasedTool; 2] = [&err, &broken];
        for e in wrappers {
            assert!(!e.name().is_empty());
            assert!(!e.description().is_empty());
            // Default REMOTE_OK / REQUIRED_ROLE on these test tools.
            let _ = e.remote_ok();
            assert!(!e.required_role().is_empty());
            assert!(e.input_schema().is_object());
            assert!(e.output_schema().is_object());
        }
    }

    #[tokio::test]
    async fn run_json_propagates_run_errors() {
        let w = ToolWrapper::<ErrTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let err = e
            .run_json(serde_json::json!({"n": 1}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn run_json_serialize_failure_returns_named_error() {
        let w = ToolWrapper::<BrokenSerializeTool>(PhantomData);
        let e: &dyn ErasedTool = &w;
        let err = e
            .run_json(serde_json::json!({"n": 1}), &ctx())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to serialize output of broken"),
            "got: {msg}"
        );
    }

    #[test]
    fn value_to_text_passes_strings_through_and_pretty_prints_others() {
        assert_eq!(value_to_text(&Value::String("hi".into())), "hi");
        let pretty = value_to_text(&serde_json::json!({"a": 1}));
        // Pretty-printed JSON has a newline; raw `.to_string()` would not.
        assert!(pretty.contains('\n'), "got: {pretty}");
        assert!(pretty.contains("\"a\""));
    }

    #[test]
    fn schema_for_handles_non_object_root_without_panicking() {
        // Real generic path: schemars always emits an object root.
        let s = schema_for::<bool>();
        let _ = s;
    }

    #[test]
    fn sanitize_schema_strips_bookkeeping_keys_from_object_root() {
        // Object root: `$schema`/`title` are removed, real keys kept, and
        // typeless properties are coerced.
        let v = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Args",
            "type": "object",
            "properties": { "free": true }
        });
        let out = sanitize_schema(v);
        assert!(out.get("$schema").is_none());
        assert!(out.get("title").is_none());
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["free"]["type"], "object");
    }

    #[test]
    fn sanitize_schema_passes_non_object_root_through() {
        // Non-object root exercises the `as_object_mut()` None branch — only
        // reachable here, not via the generic `schema_for`.
        assert_eq!(sanitize_schema(Value::Bool(true)), Value::Bool(true));
        assert_eq!(sanitize_schema(Value::Null), Value::Null);
    }

    // ── execute gate (dry-run by default) ────────────────────────────────────
    //
    // The invariant these lock down: a verb that applies changes does NOTHING
    // unless the caller explicitly opted in. A regression here silently turns
    // plans into applications, which is exactly the class of bug the gate
    // exists to prevent — so each assertion below checks *behaviour*, not just
    // the returned shape.

    /// Targets whose body actually ran. A shared bool would be contaminated by
    /// tests running in parallel, so each test uses a UNIQUE target and asserts
    /// on that key alone — the gate must be provable without serializing tests.
    static RAN_TARGETS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    fn ran(target: &str) -> bool {
        RAN_TARGETS.lock().unwrap().iter().any(|t| t == target)
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct GatedArgs {
        target: String,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    struct GatedOut {
        applied: String,
    }

    struct GatedTool;

    impl OrcaToolDef for GatedTool {
        const NAME: &'static str = "test.gated";
        const DESCRIPTION: &'static str = "applies a change";
        const EXECUTE_GATED: bool = true;
        type Args = GatedArgs;
        type Output = GatedOut;
    }

    #[async_trait]
    impl OrcaTool for GatedTool {
        async fn run(args: GatedArgs, _ctx: &ToolCtx) -> Result<GatedOut> {
            RAN_TARGETS.lock().unwrap().push(args.target.clone());
            Ok(GatedOut {
                applied: args.target,
            })
        }
    }

    fn gated() -> ToolWrapper<GatedTool> {
        ToolWrapper(PhantomData)
    }

    // ── execute authorization (opting in is not being allowed) ───────────────

    /// An admin-only data mutation — the shape nearly every gated verb has.
    struct AdminGatedTool;

    impl OrcaToolDef for AdminGatedTool {
        const NAME: &'static str = "test.admin_gated";
        const DESCRIPTION: &'static str = "applies an admin-only change";
        const EXECUTE_GATED: bool = true;
        const REQUIRED_ROLE: &'static str = "admin";
        const DATA_MUTATION: bool = true;
        type Args = GatedArgs;
        type Output = GatedOut;
    }

    #[async_trait]
    impl OrcaTool for AdminGatedTool {
        async fn run(args: GatedArgs, _ctx: &ToolCtx) -> Result<GatedOut> {
            RAN_TARGETS.lock().unwrap().push(args.target.clone());
            Ok(GatedOut {
                applied: args.target,
            })
        }
    }

    fn admin_gated() -> ToolWrapper<AdminGatedTool> {
        ToolWrapper(PhantomData)
    }

    fn ctx_as(role: &str, can_mutate: bool) -> ToolCtx {
        let mut c = ctx();
        c.set_caller(Some(contract::CallerIdentity {
            user_id: "u-test".into(),
            username: "tester".into(),
            role: role.into(),
            can_mutate,
        }));
        c
    }

    #[tokio::test]
    async fn an_unauthorized_caller_that_opts_in_is_refused_not_quietly_planned() {
        let target = "unauthorized-execute-target";
        let err = admin_gated()
            .run_json(
                serde_json::json!({ "target": target, "execute": true }),
                &ctx_as("member", false),
            )
            .await
            .expect_err("an unauthorized execute must be an error");

        // The distinction the whole check exists for: refusing is not the same
        // as returning a plan. A plan here would report a successful dry run
        // for a call that asked to apply and was denied.
        let msg = err.to_string();
        assert!(msg.contains("requires role 'admin'"), "got: {msg}");
        assert!(msg.contains("tester"), "names the caller: {msg}");
        assert!(!ran(target), "the body must not have run");
    }

    #[tokio::test]
    async fn an_authorized_caller_that_opts_in_applies() {
        let target = "authorized-execute-target";
        let out = admin_gated()
            .run_json(
                serde_json::json!({ "target": target, "execute": true }),
                &ctx_as("admin", false),
            )
            .await
            .expect("an admin may execute");
        assert_eq!(out["applied"], serde_json::json!(target));
        assert!(ran(target));
    }

    #[tokio::test]
    async fn the_can_mutate_capability_authorizes_a_non_admin_data_mutation() {
        // Same escape hatch the REST middleware already honoured; moving the
        // check to dispatch must not narrow it, or every capability-scoped
        // token in the fleet stops working.
        let target = "can-mutate-execute-target";
        admin_gated()
            .run_json(
                serde_json::json!({ "target": target, "execute": true }),
                &ctx_as("member", true),
            )
            .await
            .expect("can_mutate authorizes a data mutation");
        assert!(ran(target));
    }

    #[tokio::test]
    async fn an_unauthorized_caller_can_still_ask_what_would_happen() {
        // Planning is a read. Denying it would push operators toward executing
        // blind, which is the opposite of what the gate is for.
        let target = "unauthorized-plan-target";
        let out = gated()
            .run_json(
                serde_json::json!({ "target": target }),
                &ctx_as("member", false),
            )
            .await
            .expect("a plan needs no execute permission");
        assert_eq!(out["dryRun"], serde_json::json!(true));
        assert!(!ran(target));
    }

    #[tokio::test]
    async fn a_dry_run_with_invalid_args_is_refused_not_planned() {
        let err = gated()
            .run_json(serde_json::json!({ "target": 7 }), &ctx())
            .await
            .expect_err("a plan for args the verb would refuse must be an error");
        assert_invalid(&err, "invalid args for test.gated");
    }

    struct EchoHint;

    impl contract::ArgRefusalHint for EchoHint {
        fn hint(&self, _ctx: &ToolCtx, message: &str) -> Option<String> {
            message.contains("target").then(|| "hinted".to_string())
        }
    }

    #[tokio::test]
    async fn a_registered_hint_extends_an_args_refusal() {
        let mut ctx = ctx();
        ctx.register_service::<Arc<dyn contract::ArgRefusalHint>>(Arc::new(EchoHint));
        let err = gated()
            .run_json(serde_json::json!({ "target": 7 }), &ctx)
            .await
            .expect_err("invalid args");
        assert_invalid(&err, "invalid args for test.gated: target: ");
        assert_invalid(&err, "; hinted");
    }

    #[tokio::test]
    async fn gated_tool_without_opt_in_does_not_run_and_returns_a_plan() {
        let target = "no-opt-in-target";
        let ctx = ctx();
        let out = gated()
            .run_json(serde_json::json!({ "target": target }), &ctx)
            .await
            .expect("planning must succeed, not error");

        assert!(!ran(target), "the tool body ran despite no execute opt-in");
        assert_eq!(out["dryRun"], serde_json::json!(true));
        assert_eq!(out["tool"], serde_json::json!("test.gated"));
        // Inputs are echoed so an operator can confirm before opting in.
        assert_eq!(out["inputs"]["target"], serde_json::json!(target));
        // Generic plans must not claim detail they do not have.
        assert_eq!(out["detailed"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn gated_tool_with_execute_true_actually_runs() {
        let target = "opt-in-target";
        let ctx = ctx();
        let out = gated()
            .run_json(
                serde_json::json!({ "target": target, "execute": true }),
                &ctx,
            )
            .await
            .expect("execute must run the body");

        assert!(ran(target), "opt-in was given but the body never ran");
        // Real output, not a plan.
        assert_eq!(out["applied"], serde_json::json!(target));
        assert!(
            out.get("dryRun").is_none(),
            "applied run must not look like a plan"
        );
    }

    #[tokio::test]
    async fn execute_is_stripped_before_typed_deserialization() {
        // GatedArgs is `deny_unknown_fields`: if the gate leaked `execute`
        // through, this would fail to deserialize. That makes the strip a
        // hard requirement, not a tidiness choice.
        let ctx = ctx();
        let out = gated()
            .run_json(
                serde_json::json!({ "target": "strip-target", "execute": true }),
                &ctx,
            )
            .await
            .expect("execute must not reach the verb's typed Args");
        assert_eq!(out["applied"], serde_json::json!("strip-target"));
    }

    #[tokio::test]
    async fn only_boolean_true_counts_as_consent() {
        let ctx = ctx();
        // A truthy-looking string is NOT consent — a caller fumbling the type
        // must get a plan, never an application.
        for (i, value) in [
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!(false),
            serde_json::json!(null),
        ]
        .into_iter()
        .enumerate()
        {
            let target = format!("not-consent-{i}");
            let args = serde_json::json!({ "target": target, "execute": value });
            let out = gated().run_json(args.clone(), &ctx).await.unwrap();
            assert_eq!(
                out["dryRun"],
                serde_json::json!(true),
                "{args} was treated as consent"
            );
            assert!(!ran(&target), "{args} caused the body to run");
        }
    }

    #[test]
    fn gated_tool_advertises_the_execute_opt_in_and_ungated_does_not() {
        let gated_schema = gated().input_schema();
        assert!(
            gated_schema["properties"]["execute"]["type"] == serde_json::json!("boolean"),
            "gated tool must document its opt-in: {gated_schema}"
        );
        // The verb's own field survives the injection.
        assert!(gated_schema["properties"]["target"].is_object());

        let plain = ToolWrapper::<DoubleTool>(PhantomData).input_schema();
        assert!(
            plain["properties"].get("execute").is_none(),
            "ungated tool must not advertise an opt-in it does not honour"
        );
    }

    #[test]
    fn execute_gated_is_forwarded_and_defaults_off() {
        assert!(gated().execute_gated());
        assert!(
            !ToolWrapper::<DoubleTool>(PhantomData).execute_gated(),
            "EXECUTE_GATED must default off so existing tools are unaffected"
        );
    }
}
