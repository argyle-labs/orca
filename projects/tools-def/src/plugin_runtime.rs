//! Plugin runtime KV — typed tool defs for `get_plugin_data` and
//! `set_plugin_data`. Values are arbitrary JSON; the underlying TEXT column
//! holds the JSON-stringified form but callers work with structured data.
//!
//! `serde_json::Value` is used intentionally here — the plugin KV store is
//! free-form by contract; each plugin defines its own per-key schema.
#![allow(clippy::disallowed_types)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::orca_tool;

#[cfg_attr(feature = "cli", derive(clap::Args))]
#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataArgs {
    pub plugin: String,
    pub key: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct GetPluginDataOutput {
    /// Stored value — arbitrary JSON. Stored as TEXT in orca.db; the
    /// host parses/serializes at the edge so callers never see a string.
    pub value: Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataArgs {
    pub plugin: String,
    pub key: String,
    /// Arbitrary JSON value — the host serializes it to TEXT at the storage edge.
    pub value: Value,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct SetPluginDataOutput {
    pub ok: bool,
}

#[cfg(feature = "native")]
fn pr(
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<std::sync::Arc<dyn crate::services::plugin_runtime::PluginRuntimeService>> {
    ctx.service::<std::sync::Arc<dyn crate::services::plugin_runtime::PluginRuntimeService>>()
}

/// Read a single key from a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin-data", verb = "get")]
async fn get_plugin_data(
    args: GetPluginDataArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<GetPluginDataOutput> {
    let value = pr(ctx)?.get(&args.plugin, &args.key).await?;
    Ok(GetPluginDataOutput { value })
}

/// [MUTATES STATE] Upsert a single key in a plugin's encrypted KV store in orca.db.
#[orca_tool(domain = "plugin-data", verb = "set", cli = skip)]
async fn set_plugin_data(
    args: SetPluginDataArgs,
    ctx: &orca_utils::tool::ToolCtx,
) -> anyhow::Result<SetPluginDataOutput> {
    pr(ctx)?.set(&args.plugin, &args.key, &args.value).await?;
    Ok(SetPluginDataOutput { ok: true })
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use crate::services::plugin_runtime::PluginRuntimeService;
    use crate::test_support::empty_ctx;
    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct StubKv {
        store: Mutex<std::collections::HashMap<(String, String), Value>>,
    }
    #[async_trait]
    impl PluginRuntimeService for StubKv {
        async fn get(&self, plugin: &str, key: &str) -> Result<Value> {
            self.store
                .lock()
                .unwrap()
                .get(&(plugin.to_string(), key.to_string()))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing {plugin}/{key}"))
        }
        async fn set(&self, plugin: &str, key: &str, value: &Value) -> Result<()> {
            self.store
                .lock()
                .unwrap()
                .insert((plugin.to_string(), key.to_string()), value.clone());
            Ok(())
        }
    }

    fn ctx_with_stub() -> orca_utils::tool::ToolCtx {
        let svc: Arc<dyn PluginRuntimeService> = Arc::new(StubKv::default());
        let mut ctx = empty_ctx();
        ctx.register_service(svc);
        ctx
    }

    #[tokio::test]
    async fn set_then_get_roundtrips_value() {
        let ctx = ctx_with_stub();
        let val = json!({"hello": "world", "n": 42});
        let setr = set_plugin_data(
            SetPluginDataArgs {
                plugin: "foo".into(),
                key: "k".into(),
                value: val.clone(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert!(setr.ok);
        let got = get_plugin_data(
            GetPluginDataArgs {
                plugin: "foo".into(),
                key: "k".into(),
            },
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(got.value, val);
    }

    #[tokio::test]
    async fn get_missing_key_errors() {
        let ctx = ctx_with_stub();
        let err = get_plugin_data(
            GetPluginDataArgs {
                plugin: "foo".into(),
                key: "nope".into(),
            },
            &ctx,
        )
        .await
        .err();
        assert!(err.is_some());
    }

    #[tokio::test]
    async fn unregistered_service_errors() {
        let ctx = empty_ctx();
        assert!(
            get_plugin_data(
                GetPluginDataArgs {
                    plugin: "a".into(),
                    key: "b".into(),
                },
                &ctx,
            )
            .await
            .is_err()
        );
    }
}
