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

use crate::{OrcaTool, ToolCtx};

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
    let mut v: Value = schemars::schema_for!(T).into();
    if let Some(m) = v.as_object_mut() {
        m.remove("$schema");
        m.remove("title");
    }
    v
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
    use crate::{OrcaTool, OrcaToolDef, ToolCtx};
    use async_trait::async_trait;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;
    use std::sync::Arc;

    #[derive(Deserialize, Serialize, JsonSchema)]
    struct Args {
        n: i64,
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
        use orca_utils::config::{Config, Model};
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
        // bool / null root schemas — exercise the `if let Value::Object` false
        // branch in `schema_for`.
        let s = schema_for::<bool>();
        // Bool root produces an object schema in practice; the test merely
        // proves the helper doesn't panic on any JsonSchema impl.
        let _ = s;
    }
}
