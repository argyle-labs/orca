#![allow(
    clippy::disallowed_types,
    reason = "untyped JSON at the plugin invoke boundary"
)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TemplateLayer {
    Workload,
    Host,
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct TemplateKindDecl {
    pub kind: String,
    #[serde(default)]
    pub layers: Vec<TemplateLayer>,
    #[serde(default)]
    pub params_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub unsupported_fields: Vec<String>,
    #[serde(default)]
    pub runtimes: Vec<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct TargetDescriptor {
    pub id: String,
    pub runtime: String,
    #[serde(default)]
    pub host_id: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct TemplateRef {
    pub name: String,
    pub version: String,
}

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn parse(s: impl Into<String>) -> std::result::Result<Self, String> {
        let s = s.into();
        let valid = s.split_once("://").is_some_and(|(scheme, rest)| {
            !scheme.is_empty()
                && !rest.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
        if valid {
            Ok(Self(s))
        } else {
            Err("secret_ref must be a reference of the form <scheme>://...".into())
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::parse(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct CredentialSlotRef {
    pub slot: String,
    pub secret_ref: SecretRef,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RenderRequest {
    pub template: TemplateRef,
    pub template_body: serde_json::Value,
    #[serde(default)]
    pub overrides: serde_json::Value,
    #[serde(default)]
    pub credentials: Vec<CredentialSlotRef>,
    pub target: TargetDescriptor,
    pub layer: TemplateLayer,
    pub instance_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct PlanStep {
    pub action: String,
    pub target: String,
    #[serde(default)]
    pub detail: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct RenderedPayload {
    pub native: serde_json::Value,
    #[serde(default)]
    pub plan: Vec<PlanStep>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ApplyRequest {
    pub template: TemplateRef,
    pub rendered: RenderedPayload,
    #[serde(default)]
    pub credentials: Vec<CredentialSlotRef>,
    pub target: TargetDescriptor,
    pub layer: TemplateLayer,
    pub instance_id: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct ApplyOutcome {
    pub executed: bool,
    #[serde(default)]
    pub detail: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StatusRequest {
    pub instance_id: String,
    pub target: TargetDescriptor,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StatusOutcome {
    pub healthy: bool,
    #[serde(default)]
    pub detail: serde_json::Value,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct DestroyRequest {
    pub instance_id: String,
    pub target: TargetDescriptor,
    #[serde(default)]
    pub dry_run: bool,
}

pub trait TemplateKindProvider: Send + Sync {
    fn name(&self) -> &str;

    fn decl(&self) -> BoxFuture<'_, Result<TemplateKindDecl>>;

    fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>>;

    fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>>;

    fn status(&self, req: StatusRequest) -> BoxFuture<'_, Result<StatusOutcome>>;

    fn destroy(&self, req: DestroyRequest) -> BoxFuture<'_, Result<ApplyOutcome>>;
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
pub const STATUS_OP: &str = "status";
pub const DESTROY_OP: &str = "destroy";

const UNSUPPORTED_LAYER: &str = "unsupported layer";

fn redact_outcome(mut out: ApplyOutcome) -> ApplyOutcome {
    utils::scrub::redact_json_in_place(&mut out.detail);
    out
}

fn redact_status(mut out: StatusOutcome) -> StatusOutcome {
    utils::scrub::redact_json_in_place(&mut out.detail);
    out
}

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
            let req: RenderRequest = dec(op, args)?;
            if req.layer == TemplateLayer::Unknown {
                return Err(err(UNSUPPORTED_LAYER));
            }
            let out = provider
                .render(req)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        APPLY_OP => {
            let req: ApplyRequest = dec(op, args)?;
            if req.layer == TemplateLayer::Unknown {
                return Err(err(UNSUPPORTED_LAYER));
            }
            let out = provider
                .apply(req)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(redact_outcome(out)).map_err(|e| err(e.to_string()))
        }
        STATUS_OP => {
            let out = provider
                .status(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(redact_status(out)).map_err(|e| err(e.to_string()))
        }
        DESTROY_OP => {
            let out = provider
                .destroy(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(redact_outcome(out)).map_err(|e| err(e.to_string()))
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
        args: serde_json::Result<serde_json::Value>,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let args =
                args.map_err(|e| anyhow::anyhow!("template_kind '{name}' {op} encode args: {e}"))?;
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
        self.call(DECL_OP, Ok(serde_json::json!({})))
    }

    fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>> {
        if req.layer == TemplateLayer::Unknown {
            return Box::pin(async { Err(anyhow::anyhow!(UNSUPPORTED_LAYER)) });
        }
        self.call(RENDER_OP, serde_json::to_value(&req))
    }

    fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
        if req.layer == TemplateLayer::Unknown {
            return Box::pin(async { Err(anyhow::anyhow!(UNSUPPORTED_LAYER)) });
        }
        let fut = self.call(APPLY_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
    }

    fn status(&self, req: StatusRequest) -> BoxFuture<'_, Result<StatusOutcome>> {
        let fut = self.call(STATUS_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_status) })
    }

    fn destroy(&self, req: DestroyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
        let fut = self.call(DESTROY_OP, serde_json::to_value(&req));
        Box::pin(async move { fut.await.map(redact_outcome) })
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
                    params_schema: Some(serde_json::json!({"type": "object"})),
                    unsupported_fields: vec![],
                    runtimes: vec!["docker".into()],
                })
            })
        }
        fn render(&self, req: RenderRequest) -> BoxFuture<'_, Result<RenderedPayload>> {
            Box::pin(async move {
                Ok(RenderedPayload {
                    native: req.template_body,
                    plan: vec![step()],
                })
            })
        }
        fn apply(&self, req: ApplyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
            Box::pin(async move {
                Ok(ApplyOutcome {
                    executed: !req.dry_run,
                    detail: req.rendered.native,
                })
            })
        }
        fn status(&self, req: StatusRequest) -> BoxFuture<'_, Result<StatusOutcome>> {
            Box::pin(async move {
                Ok(StatusOutcome {
                    healthy: true,
                    detail: serde_json::json!({"instance": req.instance_id}),
                })
            })
        }
        fn destroy(&self, req: DestroyRequest) -> BoxFuture<'_, Result<ApplyOutcome>> {
            Box::pin(async move {
                Ok(ApplyOutcome {
                    executed: !req.dry_run,
                    detail: serde_json::Value::Null,
                })
            })
        }
    }

    fn fake() -> FakeKind {
        FakeKind {
            name: "template-kind-fake".into(),
            kind: "compose".into(),
        }
    }

    fn target() -> TargetDescriptor {
        TargetDescriptor {
            id: "0192f0a0-0000-7000-8000-000000000001".into(),
            runtime: "docker".into(),
            host_id: Some("0192f0a0-0000-7000-8000-000000000002".into()),
        }
    }

    fn template_ref() -> TemplateRef {
        TemplateRef {
            name: "web".into(),
            version: "1.0.0".into(),
        }
    }

    fn step() -> PlanStep {
        PlanStep {
            action: "create".into(),
            target: "web".into(),
            detail: serde_json::json!({"image": "nginx"}),
        }
    }

    fn creds() -> Vec<CredentialSlotRef> {
        vec![CredentialSlotRef {
            slot: "db_password".into(),
            secret_ref: SecretRef::parse("op://orca/web/password").unwrap(),
        }]
    }

    fn render_req() -> RenderRequest {
        RenderRequest {
            template: template_ref(),
            template_body: serde_json::json!({"image": "nginx"}),
            overrides: serde_json::json!({"port": 8080}),
            credentials: creds(),
            target: target(),
            layer: TemplateLayer::Workload,
            instance_id: "inst-1".into(),
        }
    }

    fn apply_req(dry_run: bool) -> ApplyRequest {
        ApplyRequest {
            template: template_ref(),
            rendered: RenderedPayload {
                native: serde_json::json!({"a": 1}),
                plan: vec![step()],
            },
            credentials: creds(),
            target: target(),
            layer: TemplateLayer::Workload,
            instance_id: "inst-1".into(),
            dry_run,
        }
    }

    fn status_req() -> StatusRequest {
        StatusRequest {
            instance_id: "inst-1".into(),
            target: target(),
        }
    }

    fn destroy_req() -> DestroyRequest {
        DestroyRequest {
            instance_id: "inst-1".into(),
            target: target(),
            dry_run: true,
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

        assert!(!p.apply(apply_req(true)).await.unwrap().executed);
        assert!(p.apply(apply_req(false)).await.unwrap().executed);

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
        round_trip(&TemplateKindDecl {
            kind: "compose".into(),
            layers: vec![TemplateLayer::Workload, TemplateLayer::Host],
            params_schema: Some(serde_json::json!({"type": "object"})),
            unsupported_fields: vec!["build".into()],
            runtimes: vec!["docker".into()],
        });
        round_trip(&render_req());
        round_trip(&apply_req(true));
        round_trip(&status_req());
        round_trip(&destroy_req());
        round_trip(&StatusOutcome {
            healthy: true,
            detail: serde_json::json!({"up": 1}),
        });
        round_trip(&ApplyOutcome {
            executed: true,
            detail: serde_json::json!({"id": "abc"}),
        });
    }

    #[test]
    fn template_kind_tolerant_decode() {
        let decl: TemplateKindDecl = serde_json::from_value(serde_json::json!({
            "kind": "compose",
            "layers": ["workload", "vm-image"],
        }))
        .unwrap();
        assert_eq!(
            decl.layers,
            vec![TemplateLayer::Workload, TemplateLayer::Unknown]
        );
        assert!(decl.params_schema.is_none());
        assert!(decl.unsupported_fields.is_empty());
        assert!(decl.runtimes.is_empty());

        let payload: RenderedPayload =
            serde_json::from_value(serde_json::json!({"native": {}})).unwrap();
        assert!(payload.plan.is_empty());

        let step: PlanStep =
            serde_json::from_value(serde_json::json!({"action": "create", "target": "web"}))
                .unwrap();
        assert!(step.detail.is_null());

        let apply: ApplyRequest = serde_json::from_value(serde_json::json!({
            "template": {"name": "web", "version": "1"},
            "rendered": {"native": {}},
            "target": {"id": "t", "runtime": "docker"},
            "layer": "future-layer",
            "instance_id": "i",
        }))
        .unwrap();
        assert!(!apply.dry_run);
        assert!(apply.credentials.is_empty());
        assert_eq!(apply.layer, TemplateLayer::Unknown);
        assert!(apply.target.host_id.is_none());
    }

    #[tokio::test]
    async fn template_kind_dispatch_op_routes_every_op() {
        let p = fake();
        let decl = dispatch_op(&p, DECL_OP, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(decl["kind"], "compose");

        let rendered = dispatch_op(&p, RENDER_OP, serde_json::to_value(render_req()).unwrap())
            .await
            .unwrap();
        assert_eq!(rendered["native"]["image"], "nginx");
        assert_eq!(rendered["plan"][0]["action"], "create");

        let applied = dispatch_op(
            &p,
            APPLY_OP,
            serde_json::to_value(apply_req(false)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(applied["executed"], true);

        let status = dispatch_op(&p, STATUS_OP, serde_json::to_value(status_req()).unwrap())
            .await
            .unwrap();
        assert_eq!(status["healthy"], true);
        assert_eq!(status["detail"]["instance"], "inst-1");

        let destroyed = dispatch_op(&p, DESTROY_OP, serde_json::to_value(destroy_req()).unwrap())
            .await
            .unwrap();
        assert_eq!(destroyed["executed"], false);
    }

    #[test]
    fn template_kind_secret_ref_accepts_only_references() {
        let ok: SecretRef =
            serde_json::from_value(serde_json::json!("op://Vault/Item/field")).unwrap();
        assert_eq!(ok.as_str(), "op://Vault/Item/field");
        assert!(serde_json::from_value::<SecretRef>(serde_json::json!("hunter2")).is_err());
        assert!(serde_json::from_value::<SecretRef>(serde_json::json!("")).is_err());
    }

    #[tokio::test]
    async fn template_kind_dispatch_op_rejects_unknown_layer() {
        let mut render = render_req();
        render.layer = TemplateLayer::Unknown;
        let err = dispatch_op(&fake(), RENDER_OP, serde_json::to_value(render).unwrap())
            .await
            .unwrap_err();
        assert_eq!(err, serde_json::json!(UNSUPPORTED_LAYER));

        let mut apply = apply_req(false);
        apply.layer = TemplateLayer::Unknown;
        let err = dispatch_op(&fake(), APPLY_OP, serde_json::to_value(apply).unwrap())
            .await
            .unwrap_err();
        assert_eq!(err, serde_json::json!(UNSUPPORTED_LAYER));
    }

    #[tokio::test]
    async fn template_kind_dispatch_op_redacts_detail() {
        let mut apply = apply_req(false);
        apply.rendered.native = serde_json::json!({"password": "hunter2", "id": "abc"});
        let out = dispatch_op(&fake(), APPLY_OP, serde_json::to_value(apply).unwrap())
            .await
            .unwrap();
        assert_ne!(out["detail"]["password"], "hunter2");
        assert_eq!(out["detail"]["id"], "abc");
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn template_kind_proxy_rejects_unknown_layer() {
        let invoke: InvokeThunk = Arc::new(|_, _| panic!("unknown layer must not reach the wire"));
        let proxy = TemplateKindProxy {
            name: "template-kind-proxy-layer".into(),
            invoke,
        };
        let mut render = render_req();
        render.layer = TemplateLayer::Unknown;
        let e = proxy.render(render).await.unwrap_err();
        assert!(e.to_string().contains(UNSUPPORTED_LAYER));
        let mut apply = apply_req(false);
        apply.layer = TemplateLayer::Unknown;
        let e = proxy.apply(apply).await.unwrap_err();
        assert!(e.to_string().contains(UNSUPPORTED_LAYER));
    }

    #[tokio::test]
    async fn template_kind_dispatch_op_unknown_op() {
        let err = dispatch_op(&fake(), "explode", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            err.as_str()
                .unwrap()
                .contains("unknown template_kind op: explode")
        );
    }

    #[tokio::test]
    async fn template_kind_dispatch_op_decode_error() {
        let err = dispatch_op(&fake(), STATUS_OP, serde_json::json!({"nope": 1}))
            .await
            .unwrap_err();
        assert!(err.as_str().unwrap().starts_with("decode status args:"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn template_kind_proxy_sends_every_op_on_the_wire() {
        let calls: Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = calls.clone();
        let invoke: InvokeThunk = Arc::new(move |op, args| {
            rec.lock().unwrap().push((op.to_string(), args));
            Ok(match op {
                DECL_OP => serde_json::json!({"kind": "compose"}),
                RENDER_OP => serde_json::json!({"native": {}}),
                STATUS_OP => serde_json::json!({"healthy": false}),
                _ => serde_json::json!({"executed": false}),
            })
        });
        let proxy = TemplateKindProxy {
            name: "template-kind-proxy".into(),
            invoke,
        };

        assert_eq!(proxy.decl().await.unwrap().kind, "compose");
        assert!(proxy.render(render_req()).await.unwrap().plan.is_empty());
        assert!(!proxy.apply(apply_req(true)).await.unwrap().executed);
        assert!(!proxy.status(status_req()).await.unwrap().healthy);
        assert!(!proxy.destroy(destroy_req()).await.unwrap().executed);

        let calls = calls.lock().unwrap();
        let ops: Vec<&str> = calls.iter().map(|(op, _)| op.as_str()).collect();
        assert_eq!(ops, vec!["decl", "render", "apply", "status", "destroy"]);
        assert_eq!(calls[0].1, serde_json::json!({}));
        assert_eq!(calls[1].1, serde_json::to_value(render_req()).unwrap());
        assert_eq!(calls[2].1, serde_json::to_value(apply_req(true)).unwrap());
        assert_eq!(calls[2].1["dry_run"], true);
        assert_eq!(
            calls[2].1["credentials"][0]["secret_ref"],
            "op://orca/web/password"
        );
        assert_eq!(calls[3].1, serde_json::to_value(status_req()).unwrap());
        assert_eq!(calls[4].1, serde_json::to_value(destroy_req()).unwrap());
    }
}
