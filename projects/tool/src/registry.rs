//! ToolRegistry — single source of truth for all registered OrcaTool impls.
//!
//! One registry instance drives:
//!   - MCP:  mcp_definitions() → tools/list,  dispatch() → tools/call
//!   - HTTP: axum_router() → one POST route per tool  (returns Router, caller mounts it)
//!   - CLI:  clap_command() + cli_dispatch() → `orca exec <name> [flags]`

use anyhow::Result;
use serde_json::{Value, json};
use std::marker::PhantomData;

use crate::{OrcaTool, ToolCtx};
use crate::erased::{ErasedTool, ToolWrapper};

pub struct ToolRegistry {
    tools: Vec<Box<dyn ErasedTool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool. Panics at startup (not runtime) if NAME collides with an existing tool.
    pub fn register<T: OrcaTool>(&mut self) -> &mut Self {
        let name = T::NAME;
        assert!(
            !self.tools.iter().any(|t| t.name() == name),
            "duplicate tool name: {name}"
        );
        self.tools.push(Box::new(ToolWrapper::<T>(PhantomData)));
        self
    }

    // ── MCP ──────────────────────────────────────────────────────────────────

    /// Build the JSON array for `tools/list`.
    pub fn mcp_definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "inputSchema": t.schema(),
                })
            })
            .collect()
    }

    /// Dispatch a `tools/call` by name. Returns `Err` for unknown tool names.
    pub async fn dispatch(&self, name: &str, args: Value, ctx: &ToolCtx) -> Result<String> {
        match self.tools.iter().find(|t| t.name() == name) {
            Some(tool) => tool.run_json(args, ctx).await,
            None => anyhow::bail!("unknown tool: {name}"),
        }
    }

    // ── CLI ───────────────────────────────────────────────────────────────────

    /// Returns all registered tool names — used to build CLI help text.
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Execute a tool by name, accepting args as a JSON string or `key=value` pairs.
    ///
    /// Used by `orca exec <name> [--json '{...}' | key=value ...]`.
    pub async fn cli_dispatch(
        &self,
        name: &str,
        raw_args: CliArgs,
        ctx: &ToolCtx,
    ) -> Result<String> {
        let args_json = match raw_args {
            CliArgs::Json(s) => serde_json::from_str(&s)
                .map_err(|e| anyhow::anyhow!("invalid JSON args: {e}"))?,
            CliArgs::Pairs(pairs) => {
                let mut map = serde_json::Map::new();
                for pair in pairs {
                    let (k, v) = pair
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("expected key=value, got: {pair}"))?;
                    // Try to parse the value as JSON first (handles booleans, numbers, etc.),
                    // fall back to plain string.
                    let val: Value = serde_json::from_str(v).unwrap_or(Value::String(v.to_string()));
                    map.insert(k.to_string(), val);
                }
                Value::Object(map)
            }
        };
        self.dispatch(name, args_json, ctx).await
    }
}

/// How the CLI passes arguments to a tool.
pub enum CliArgs {
    /// `--json '{"mode":"hybrid"}'`
    Json(String),
    /// `mode=hybrid enabled=true`
    Pairs(Vec<String>),
}
