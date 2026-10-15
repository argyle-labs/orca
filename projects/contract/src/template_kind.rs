#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::BoxFuture;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TemplateLayer {
    Workload,
    Host,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct TemplateKindDecl {
    pub kind: String,
    pub layers: Vec<TemplateLayer>,
    pub params_schema: serde_json::Value,
    #[serde(default)]
    pub unsupported_fields: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RenderRequest {
    pub template_body: serde_json::Value,
    #[serde(default)]
    pub overrides: serde_json::Value,
    pub target_id: Uuid,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RenderedPayload {
    pub native: serde_json::Value,
    #[serde(default)]
    pub plan: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ApplyRequest {
    pub rendered: RenderedPayload,
    pub target_id: Uuid,
    pub execute: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ApplyOutcome {
    pub executed: bool,
    #[serde(default)]
    pub detail: serde_json::Value,
}

pub trait TemplateKindProvider: Send + Sync {
    fn name(&self) -> &str;

    fn decl(&self) -> BoxFuture<'_, Result<TemplateKindDecl>>;

    fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>>;

    fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn TemplateKindProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub fn register_provider(provider: Arc<dyn TemplateKindProvider>) {
    let mut g = GLOBAL.write().expect("template_kind registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

pub fn providers() -> Vec<Arc<dyn TemplateKindProvider>> {
    GLOBAL
        .read()
        .expect("template_kind registry poisoned")
        .clone()
}

pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("template_kind registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(&str, serde_json::Value) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

pub const DECL_OP: &str = "decl";
pub const RENDER_OP: &str = "render";
pub const APPLY_OP: &str = "apply";

pub async fn dispatch_op(
    provider: &dyn TemplateKindProvider,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    fn err(msg: impl Into<String>) -> serde_json::Value {
        serde_json::Value::String(msg.into())
    }
    fn dec<T: serde::de::DeserializeOwned>(
        op: &str,
        args: serde_json::Value,
    ) -> std::result::Result<T, serde_json::Value> {
        serde_json::from_value(args).map_err(|e| err(format!("decode {op} args: {e}")))
    }
    match op {
        DECL_OP => {
            let out = provider.decl().await.map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        RENDER_OP => {
            let out = provider
                .render(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        APPLY_OP => {
            let out = provider
                .apply(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        other => Err(err(format!("unknown template_kind op: {other}"))),
    }
}

#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(TemplateKindProxy { name, invoke }));
    Ok(())
}

#[cfg(feature = "in-process")]
struct TemplateKindProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl TemplateKindProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args))
                .await
                .map_err(|e| anyhow::anyhow!("template_kind '{name}' {op} task panicked: {e}"))?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "template_kind '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out).map_err(|e| {
                anyhow::anyhow!("template_kind '{name}' {op} returned invalid JSON: {e}")
            })
        })
    }
}

#[cfg(feature = "in-process")]
impl TemplateKindProvider for TemplateKindProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn decl(&self) -> BoxFuture<'_, Result<TemplateKindDecl>> {
        self.call(DECL_OP, serde_json::json!({}))
    }

    fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>> {
        let args = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));
        self.call(RENDER_OP, args)
    }

    fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
        let args = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));
        self.call(APPLY_OP, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeKind {
        name: String,
        kind: String,
    }

    impl TemplateKindProvider for FakeKind {
        fn name(&self) -> &str {
            &self.name
        }
        fn decl(&self) -> BoxFuture<'_, Result<TemplateKindDecl>> {
            let kind = self.kind.clone();
            Box::pin(async move {
                Ok(TemplateKindDecl {
                    kind,
                    layers: vec![TemplateLayer::Workload],
                    params_schema: serde_json::json!({"type": "object"}),
                    unsupported_fields: vec![],
                })
            })
        }
        fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>> {
            Box::pin(async move {
                Ok(RenderedPayload {
                    native: req.template_body,
                    plan: vec!["render".into()],
                })
            })
        }
        fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
            Box::pin(async move {
                Ok(ApplyOutcome {
                    executed: req.execute,
                    detail: req.rendered.native,
                })
            })
        }
    }

    fn find(name: &str) -> Option<Arc<dyn TemplateKindProvider>> {
        providers().into_iter().find(|p| p.name() == name)
    }

    #[tokio::test]
    async fn template_kind_register_replace_deregister() {
        register_provider(Arc::new(FakeKind {
            name: "template-kind-test".into(),
            kind: "v1".into(),
        }));
        let p = find("template-kind-test").expect("registered");
        assert_eq!(p.decl().await.unwrap().kind, "v1");

        register_provider(Arc::new(FakeKind {
            name: "template-kind-test".into(),
            kind: "v2".into(),
        }));
        assert_eq!(
            providers()
                .iter()
                .filter(|p| p.name() == "template-kind-test")
                .count(),
            1
        );
        let p = find("template-kind-test").expect("replaced");
        assert_eq!(p.decl().await.unwrap().kind, "v2");

        let out = p
            .apply(ApplyRequest {
                rendered: RenderedPayload {
                    native: serde_json::json!({"a": 1}),
                    plan: vec![],
                },
                target_id: Uuid::nil(),
                execute: false,
            })
            .await
            .unwrap();
        assert!(!out.executed);

        assert!(deregister_provider("template-kind-test"));
        assert!(find("template-kind-test").is_none());
        assert!(!deregister_provider("template-kind-test"));
    }

    fn round_trip<T>(v: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(v).unwrap();
        let back: T = serde_json::from_value(json).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn template_kind_layer_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(TemplateLayer::Workload).unwrap(),
            serde_json::json!("workload")
        );
        assert_eq!(
            serde_json::to_value(TemplateLayer::Host).unwrap(),
            serde_json::json!("host")
        );
        round_trip(&TemplateLayer::Host);
    }

    #[test]
    fn template_kind_wire_types_round_trip() {
        let target_id = Uuid::now_v7();
        let rendered = RenderedPayload {
            native: serde_json::json!({"services": {}}),
            plan: vec!["create web".into()],
        };
        round_trip(&TemplateKindDecl {
            kind: "compose".into(),
            layers: vec![TemplateLayer::Workload, TemplateLayer::Host],
            params_schema: serde_json::json!({"type": "object"}),
            unsupported_fields: vec!["build".into()],
        });
        round_trip(&RenderRequest {
            template_body: serde_json::json!({"image": "nginx"}),
            overrides: serde_json::json!({"port": 8080}),
            target_id,
        });
        round_trip(&rendered);
        round_trip(&ApplyRequest {
            rendered,
            target_id,
            execute: true,
        });
        round_trip(&ApplyOutcome {
            executed: true,
            detail: serde_json::json!({"id": "abc"}),
        });
    }
}
