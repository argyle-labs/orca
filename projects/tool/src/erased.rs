//! Object-safe wrapper around OrcaTool.
//!
//! OrcaTool has an associated type and async fn, so `dyn OrcaTool` doesn't work.
//! ErasedTool erases those details so tools can live in a Vec<Box<dyn ErasedTool>>.

use anyhow::Result;
use futures::future::BoxFuture;
use serde_json::Value;
use std::marker::PhantomData;

use crate::{OrcaTool, ToolCtx};

/// Object-safe version of OrcaTool. Implemented automatically for any OrcaTool via ToolWrapper.
pub trait ErasedTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for this tool's Args — used for MCP tools/list and CLI flag generation.
    fn schema(&self) -> Value;
    /// Deserialize args from JSON, run the tool, return text output.
    fn run_json<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<String>>;
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

    fn schema(&self) -> Value {
        let root = schemars::schema_for!(T::Args);
        let mut v = serde_json::to_value(root).unwrap_or(Value::Null);
        // Strip the $schema URI — MCP doesn't use it and it adds noise.
        if let Value::Object(ref mut m) = v {
            m.remove("$schema");
            m.remove("title");
        }
        v
    }

    fn run_json<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            let parsed: T::Args = serde_json::from_value(args)
                .map_err(|e| anyhow::anyhow!("invalid args for {}: {e}", T::NAME))?;
            T::run(parsed, ctx).await
        })
    }
}
