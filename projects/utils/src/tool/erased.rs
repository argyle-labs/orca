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

use super::{OrcaTool, ToolCtx};

/// Object-safe version of OrcaTool. Implemented automatically for any OrcaTool via ToolWrapper.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
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
    let root = schemars::schema_for!(T);
    let mut v = serde_json::to_value(root).unwrap_or(Value::Null);
    if let Value::Object(ref mut m) = v {
        // Strip MCP-irrelevant noise; clients that want it can reintroduce.
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
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}
