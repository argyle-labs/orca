//! Cross-crate tunnel-provider capability types and provider registry (VPN side).
//!
//! A tunnel provider *issues* tunnel settings: it opens a session with a VPN
//! service for a client public key and, optionally, a forwarded port. The
//! firewall side ([`crate::tunnel`]) applies them. Provider credentials stay
//! inside the provider (read via orca secrets) and never cross this seam.
//!
//! The surface layer (`dispatch::tunnel_provider_surface`) routes each op to the
//! named provider. A subprocess plugin registers through [`register_from_def`]
//! for `domain = "tunnel_provider"`.

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`;
// the sanctioned opaque seam, scoped here (same as `ups`).
#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Request a tunnel session for `client_pubkey`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct SessionReq {
    /// Preferred server region; `None` = provider's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Only pick a server that supports port forwarding.
    #[serde(default)]
    pub require_port_forward: bool,
    pub client_pubkey: String,
}

/// Settings issued for one tunnel session.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct Session {
    pub endpoint_host: String,
    pub endpoint_port: u16,
    pub server_pubkey: String,
    /// Client tunnel address in CIDR form.
    pub local_address: String,
    #[serde(default)]
    pub dns: Vec<String>,
    /// Server's in-tunnel address, used for port-forward requests.
    pub server_vip: String,
    /// Server certificate common name, used for port-forward requests.
    pub server_cn: String,
    /// Unix seconds the session expires; `None` = no stated expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// Request a forwarded port on the server a [`Session`] landed on.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PortForwardReq {
    pub server_cn: String,
    pub server_vip: String,
}

/// A forwarded port and how often it must be refreshed to stay open.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PortForward {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    pub refresh_secs: u32,
}

/// Arguments to open a session, routed to the named provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct SessionArgs {
    pub provider: String,
    pub session: SessionReq,
}

/// Arguments to request a forwarded port, routed to the named provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PortForwardArgs {
    pub provider: String,
    pub port_forward: PortForwardReq,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A VPN-side settings issuer — one per provider.
pub trait TunnelSessionProvider: Send + Sync {
    /// Provider/registry name. Registry key; used to replace-in-place on
    /// re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    fn session(&self, req: SessionReq) -> BoxFuture<'_, Result<Session>>;

    fn port_forward(&self, req: PortForwardReq) -> BoxFuture<'_, Result<PortForward>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn TunnelSessionProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a provider. Re-registering the same `name()` replaces the entry.
pub fn register_provider(provider: Arc<dyn TunnelSessionProvider>) {
    let mut g = GLOBAL.write().expect("tunnel_provider registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn TunnelSessionProvider>> {
    GLOBAL
        .read()
        .expect("tunnel_provider registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("tunnel_provider registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

pub fn provider_named(name: &str) -> Result<Arc<dyn TunnelSessionProvider>> {
    providers()
        .into_iter()
        .find(|p| p.name() == name)
        .ok_or_else(|| anyhow::anyhow!("no tunnel_provider named '{name}'"))
}

// ── Surface entry points (used by dispatch::tunnel_provider_surface) ─────────

pub async fn session(args: SessionArgs) -> Result<Session> {
    provider_named(&args.provider)?.session(args.session).await
}

pub async fn port_forward(args: PortForwardArgs) -> Result<PortForward> {
    provider_named(&args.provider)?
        .port_forward(args.port_forward)
        .await
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// `(op, args) -> Result<result, error>`, all as `serde_json::Value`.
#[cfg(feature = "in-process")]
pub type InvokeThunk = Arc<
    dyn Fn(
            &str,
            serde_json::Value,
            Option<crate::invocation_scope::BackendCall>,
        ) -> std::result::Result<serde_json::Value, serde_json::Value>
        + Send
        + Sync
        + 'static,
>;

/// Operation names invoked across the FFI boundary as
/// `"{invoke_prefix}.{SESSION_OP|PORT_FORWARD_OP}"`.
pub const SESSION_OP: &str = "session";
pub const PORT_FORWARD_OP: &str = "port_forward";

/// Plugin-side dispatch: answer a proxied op by calling the typed provider.
pub async fn dispatch_op(
    provider: &dyn TunnelSessionProvider,
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
        SESSION_OP => {
            let out = provider
                .session(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        PORT_FORWARD_OP => {
            let out = provider
                .port_forward(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        other => Err(err(format!("unknown tunnel_provider op: {other}"))),
    }
}

/// Build and register a provider from a plugin backend descriptor plus an
/// [`InvokeThunk`]. The plugin-loader calls this for `domain = "tunnel_provider"`.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(TunnelSessionProxy { name, invoke }));
    Ok(())
}

#[cfg(feature = "in-process")]
struct TunnelSessionProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl TunnelSessionProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let call = crate::invocation_scope::capture("tunnel_provider", &name);
            let out = tokio::task::spawn_blocking(move || invoke(op, args, call))
                .await
                .map_err(|e| anyhow::anyhow!("tunnel_provider '{name}' {op} task panicked: {e}"))?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "tunnel_provider '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out).map_err(|e| {
                anyhow::anyhow!("tunnel_provider '{name}' {op} returned invalid JSON: {e}")
            })
        })
    }
}

#[cfg(feature = "in-process")]
impl TunnelSessionProvider for TunnelSessionProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn session(&self, req: SessionReq) -> BoxFuture<'_, Result<Session>> {
        let args = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));
        self.call(SESSION_OP, args)
    }

    fn port_forward(&self, req: PortForwardReq) -> BoxFuture<'_, Result<PortForward>> {
        let args = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));
        self.call(PORT_FORWARD_OP, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeVpn;

    impl TunnelSessionProvider for FakeVpn {
        fn name(&self) -> &str {
            "tprov-test"
        }
        fn session(&self, req: SessionReq) -> BoxFuture<'_, Result<Session>> {
            Box::pin(async move {
                Ok(Session {
                    endpoint_host: req.region.unwrap_or_else(|| "any".into()),
                    endpoint_port: 1337,
                    server_pubkey: "spk".into(),
                    local_address: "10.0.9.2/32".into(),
                    dns: vec!["10.0.9.1".into()],
                    server_vip: "10.0.9.1".into(),
                    server_cn: "cn1".into(),
                    expires_at: None,
                })
            })
        }
        fn port_forward(&self, req: PortForwardReq) -> BoxFuture<'_, Result<PortForward>> {
            Box::pin(async move {
                assert_eq!(req.server_cn, "cn1");
                Ok(PortForward {
                    port: 51413,
                    expires_at: Some(10),
                    refresh_secs: 900,
                })
            })
        }
    }

    #[tokio::test]
    async fn routes_to_named_provider() {
        register_provider(Arc::new(FakeVpn));
        let s = session(SessionArgs {
            provider: "tprov-test".into(),
            session: SessionReq {
                region: Some("ca".into()),
                require_port_forward: true,
                client_pubkey: "cpk".into(),
            },
        })
        .await
        .expect("routes");
        assert_eq!(s.endpoint_host, "ca");

        let pf = port_forward(PortForwardArgs {
            provider: "tprov-test".into(),
            port_forward: PortForwardReq {
                server_cn: s.server_cn,
                server_vip: s.server_vip,
            },
        })
        .await
        .expect("routes");
        assert_eq!(pf.port, 51413);
        assert!(deregister_provider("tprov-test"));
    }

    #[tokio::test]
    async fn unknown_provider_errors() {
        assert!(
            session(SessionArgs {
                provider: "nope".into(),
                session: SessionReq::default(),
            })
            .await
            .is_err()
        );
    }

    #[cfg(feature = "in-process")]
    fn bridge(provider: Arc<dyn TunnelSessionProvider>) -> InvokeThunk {
        Arc::new(move |op: &str, args, _call| {
            tokio::runtime::Handle::current().block_on(dispatch_op(provider.as_ref(), op, args))
        })
    }

    #[cfg(feature = "in-process")]
    fn proxy_named(name: &str) -> Arc<dyn TunnelSessionProvider> {
        providers()
            .into_iter()
            .find(|p| p.name() == name)
            .expect("proxy registered")
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn subprocess_round_trip_covers_every_op() {
        register_from_def("tprov-rt".into(), bridge(Arc::new(FakeVpn))).unwrap();
        let proxy = proxy_named("tprov-rt");

        let s = proxy
            .session(SessionReq {
                region: Some("nl".into()),
                require_port_forward: true,
                client_pubkey: "cpk".into(),
            })
            .await
            .expect("session");
        assert_eq!(s.endpoint_host, "nl");
        assert_eq!(s.endpoint_port, 1337);

        let pf = proxy
            .port_forward(PortForwardReq {
                server_cn: s.server_cn,
                server_vip: s.server_vip,
            })
            .await
            .expect("port_forward");
        assert_eq!(pf.port, 51413);
        assert_eq!(pf.refresh_secs, 900);
        assert!(deregister_provider("tprov-rt"));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn subprocess_errors_and_bad_json_are_rendered() {
        let invoke: InvokeThunk = Arc::new(|op: &str, _args, _call| match op {
            SESSION_OP => Err(serde_json::json!("auth rejected")),
            _ => Err(serde_json::json!({"code": 7})),
        });
        register_from_def("tprov-rt-err".into(), invoke).unwrap();
        let proxy = proxy_named("tprov-rt-err");
        let err = proxy
            .session(SessionReq::default())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "tunnel_provider 'tprov-rt-err' session failed: auth rejected"
        );
        let err = proxy
            .port_forward(PortForwardReq {
                server_cn: "cn".into(),
                server_vip: "vip".into(),
            })
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            r#"tunnel_provider 'tprov-rt-err' port_forward failed: {"code":7}"#
        );
        assert!(deregister_provider("tprov-rt-err"));

        let invoke: InvokeThunk = Arc::new(|_op: &str, _args, _call| Ok(serde_json::json!(42)));
        register_from_def("tprov-rt-bad".into(), invoke).unwrap();
        let err = proxy_named("tprov-rt-bad")
            .session(SessionReq::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("tunnel_provider 'tprov-rt-bad' session returned invalid JSON:"),
            "{err}"
        );
        assert!(deregister_provider("tprov-rt-bad"));
    }

    #[tokio::test]
    async fn dispatch_op_rejects_unknown_op_and_bad_args() {
        let err = dispatch_op(&FakeVpn, "teardown", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            serde_json::json!("unknown tunnel_provider op: teardown")
        );
        let err = dispatch_op(
            &FakeVpn,
            PORT_FORWARD_OP,
            serde_json::json!({"server_cn": 1}),
        )
        .await
        .unwrap_err();
        assert!(
            err.as_str()
                .unwrap()
                .starts_with("decode port_forward args:"),
            "{err}"
        );
    }
}
