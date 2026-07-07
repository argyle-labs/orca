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

/// Object-safe version of OrcaTool. Implemented automatically for any OrcaTool via ToolWrapper.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// Whether this tool may be invoked by a paired pod peer via `pod/exec`.
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

    fn input_schema(&self) -> Value {
        schema_for::<T::Args>()
    }

    fn output_schema(&self) -> Value {
        schema_for::<T::Output>()
    }

    fn run_json<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let parsed: T::Args = serde_json::from_value(args)
                .map_err(|e| anyhow::anyhow!("invalid args for {}: {e}", T::NAME))?;
            let out = T::run(parsed, ctx).await?;
            serde_json::to_value(&out)
                .map_err(|e| anyhow::anyhow!("failed to serialize output of {}: {e}", T::NAME))
        })
    }
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
}
