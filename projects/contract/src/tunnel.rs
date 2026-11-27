//! Cross-crate tunnel capability types and provider registry (firewall side).
//!
//! A tunnel provider owns the firewall/router end of a VPN tunnel (e.g. a
//! WireGuard interface on OPNsense) and *applies* settings to it. The settings
//! themselves are issued by a [`crate::tunnel_provider`] (the VPN side).
//!
//! The surface layer (`dispatch::tunnel_surface`) fans `list` across every
//! provider, routes `apply` to the provider that owns the tunnel id and
//! `port_alias_set` to the named provider, so the surface stays plugin-agnostic.
//!
//! ## Provider registry
//!
//! A provider registers into a process-global registry — either in-process or,
//! for an external subprocess plugin, via the [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "tunnel"`.

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`;
// the sanctioned opaque seam, scoped here (same as `ups`).
#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Filter for a `list` fan-out. `None` = every tunnel.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct TunnelQuery {
    /// Restrict to a single tunnel id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Live state of one peer on a tunnel.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PeerState {
    pub id: String,
    pub endpoint: String,
    pub pubkey: String,
    /// Unix seconds of the last completed handshake; `None` = never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_handshake: Option<u64>,
}

/// Live state of one tunnel, as read from a provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct TunnelState {
    /// Provider that reported this tunnel (registry key).
    pub provider: String,
    /// Stable id of the tunnel within its provider.
    pub id: String,
    /// Interface/device name (e.g. `wg0`).
    pub device: String,
    pub enabled: bool,
    /// Local tunnel address in CIDR form.
    pub local_address: String,
    #[serde(default)]
    pub peers: Vec<PeerState>,
}

/// The remote peer a tunnel is pointed at.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PeerSpec {
    pub pubkey: String,
    pub endpoint_host: String,
    pub endpoint_port: u16,
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_secs: Option<u16>,
}

/// Settings to apply to one tunnel, routed to the provider owning `tunnel_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct TunnelApply {
    pub tunnel_id: String,
    /// Local tunnel address in CIDR form.
    pub local_address: String,
    pub peer: PeerSpec,
    #[serde(default)]
    pub dns: Vec<String>,
}

/// Result of a [`TunnelProvider::apply`].
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct TunnelApplyOutcome {
    pub changed: bool,
    /// Human-readable list of the settings that were changed.
    #[serde(default)]
    pub applied: Vec<String>,
}

/// Set a firewall port alias to exactly `ports`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PortAliasSet {
    pub alias: String,
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// Result of a [`TunnelProvider::port_alias_set`].
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct PortAliasOutcome {
    pub changed: bool,
}

/// Arguments to set a port alias, routed to the named provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct PortAliasSetArgs {
    /// Provider registry name that owns the alias.
    pub provider: String,
    pub set: PortAliasSet,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A firewall-side tunnel manager — one per provider. Registered into the
/// process-global registry so the surface layer can route plugin-agnostically.
pub trait TunnelProvider: Send + Sync {
    /// Provider/registry name. Registry key; used to replace-in-place on
    /// re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    fn list(&self, query: TunnelQuery) -> BoxFuture<'_, Result<Vec<TunnelState>>>;

    fn apply(&self, apply: TunnelApply) -> BoxFuture<'_, Result<TunnelApplyOutcome>>;

    fn port_alias_set(&self, set: PortAliasSet) -> BoxFuture<'_, Result<PortAliasOutcome>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn TunnelProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a tunnel provider. Re-registering the same `name()` replaces the
/// existing entry so a plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn TunnelProvider>) {
    let mut g = GLOBAL.write().expect("tunnel registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn TunnelProvider>> {
    GLOBAL.read().expect("tunnel registry poisoned").clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("tunnel registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points (used by dispatch::tunnel_surface) ──────────────────

/// Fan `list` across all providers. A provider that errors is skipped — one
/// broken provider must not blank the whole report.
pub async fn list(query: TunnelQuery) -> Vec<TunnelState> {
    let mut out = Vec::new();
    for p in providers() {
        if let Ok(mut s) = p.list(query.clone()).await {
            out.append(&mut s);
        }
    }
    out
}

/// The provider that reports tunnel `tunnel_id`.
pub async fn owner_of(tunnel_id: &str) -> Result<Arc<dyn TunnelProvider>> {
    let query = TunnelQuery {
        id: Some(tunnel_id.to_string()),
    };
    for p in providers() {
        if let Ok(tunnels) = p.list(query.clone()).await
            && tunnels.iter().any(|t| t.id == tunnel_id)
        {
            return Ok(p);
        }
    }
    Err(anyhow::anyhow!(
        "no tunnel provider owns tunnel '{tunnel_id}'"
    ))
}

/// The registered provider named `name`.
pub fn provider_named(name: &str) -> Result<Arc<dyn TunnelProvider>> {
    providers()
        .into_iter()
        .find(|p| p.name() == name)
        .ok_or_else(|| anyhow::anyhow!("no tunnel provider named '{name}'"))
}

/// Route one `apply` to the provider that owns `apply.tunnel_id`.
pub async fn apply(apply: TunnelApply) -> Result<TunnelApplyOutcome> {
    owner_of(&apply.tunnel_id).await?.apply(apply).await
}

/// Route one `port_alias_set` to the named provider.
pub async fn port_alias_set(args: PortAliasSetArgs) -> Result<PortAliasOutcome> {
    provider_named(&args.provider)?
        .port_alias_set(args.set)
        .await
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// `(op, args) -> Result<result, error>`, all as `serde_json::Value`. Plain `Fn`
/// so `contract` stays free of any ABI/loader dependency.
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

/// Operation names the [`TunnelProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{LIST_OP|APPLY_OP|PORT_ALIAS_SET_OP}"`.
pub const LIST_OP: &str = "list";
pub const APPLY_OP: &str = "apply";
pub const PORT_ALIAS_SET_OP: &str = "port_alias_set";

/// Plugin-side dispatch: answer a proxied tunnel op by calling the typed
/// [`TunnelProvider`]. Symmetric with the host-side proxy.
pub async fn dispatch_op(
    provider: &dyn TunnelProvider,
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
        LIST_OP => {
            let out = provider
                .list(dec(op, args)?)
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
        PORT_ALIAS_SET_OP => {
            let out = provider
                .port_alias_set(dec(op, args)?)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(&out).map_err(|e| err(e.to_string()))
        }
        other => Err(err(format!("unknown tunnel op: {other}"))),
    }
}

/// Build and register a [`TunnelProvider`] from a plugin backend descriptor plus
/// an [`InvokeThunk`]. The plugin-loader calls this for `domain = "tunnel"`.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(TunnelProxy { name, invoke }));
    Ok(())
}

/// A [`TunnelProvider`] backed by a subprocess plugin reached over the
/// JSON-proxy FFI boundary.
#[cfg(feature = "in-process")]
struct TunnelProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl TunnelProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let call = crate::invocation_scope::capture("tunnel", &name);
            let out = tokio::task::spawn_blocking(move || invoke(op, args, call))
                .await
                .map_err(|e| anyhow::anyhow!("tunnel '{name}' {op} task panicked: {e}"))?
                .map_err(|e| {
                    anyhow::anyhow!(
                        "tunnel '{name}' {op} failed: {}",
                        crate::render_invoke_error(&e)
                    )
                })?;
            serde_json::from_value(out)
                .map_err(|e| anyhow::anyhow!("tunnel '{name}' {op} returned invalid JSON: {e}"))
        })
    }
}

#[cfg(feature = "in-process")]
impl TunnelProvider for TunnelProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn list(&self, query: TunnelQuery) -> BoxFuture<'_, Result<Vec<TunnelState>>> {
        let args = serde_json::to_value(&query).unwrap_or_else(|_| serde_json::json!({}));
        self.call(LIST_OP, args)
    }

    fn apply(&self, apply: TunnelApply) -> BoxFuture<'_, Result<TunnelApplyOutcome>> {
        let args = serde_json::to_value(&apply).unwrap_or_else(|_| serde_json::json!({}));
        self.call(APPLY_OP, args)
    }

    fn port_alias_set(&self, set: PortAliasSet) -> BoxFuture<'_, Result<PortAliasOutcome>> {
        let args = serde_json::to_value(&set).unwrap_or_else(|_| serde_json::json!({}));
        self.call(PORT_ALIAS_SET_OP, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTunnel {
        name: String,
    }

    impl TunnelProvider for FakeTunnel {
        fn name(&self) -> &str {
            &self.name
        }
        fn list(&self, _query: TunnelQuery) -> BoxFuture<'_, Result<Vec<TunnelState>>> {
            let provider = self.name.clone();
            Box::pin(async move {
                Ok(vec![TunnelState {
                    provider,
                    id: "tun-test-1".into(),
                    device: "wg0".into(),
                    enabled: true,
                    local_address: "10.0.9.2/32".into(),
                    peers: vec![],
                }])
            })
        }
        fn apply(&self, apply: TunnelApply) -> BoxFuture<'_, Result<TunnelApplyOutcome>> {
            Box::pin(async move {
                Ok(TunnelApplyOutcome {
                    changed: true,
                    applied: vec![format!("local_address={}", apply.local_address)],
                })
            })
        }
        fn port_alias_set(&self, _set: PortAliasSet) -> BoxFuture<'_, Result<PortAliasOutcome>> {
            Box::pin(async move { Ok(PortAliasOutcome { changed: true }) })
        }
    }

    fn sample_apply(tunnel_id: &str) -> TunnelApply {
        TunnelApply {
            tunnel_id: tunnel_id.into(),
            local_address: "10.0.9.2/32".into(),
            peer: PeerSpec {
                pubkey: "pk".into(),
                endpoint_host: "vpn.example".into(),
                endpoint_port: 51820,
                allowed_ips: vec!["0.0.0.0/0".into()],
                keepalive_secs: Some(25),
            },
            dns: vec!["10.0.9.1".into()],
        }
    }

    #[tokio::test]
    async fn fan_out_and_route() {
        register_provider(Arc::new(FakeTunnel {
            name: "tunnel-test".into(),
        }));
        let s = list(TunnelQuery::default()).await;
        assert!(s.iter().any(|t| t.provider == "tunnel-test" && t.enabled));

        let out = apply(sample_apply("tun-test-1")).await.expect("routes");
        assert!(out.changed);
        assert_eq!(out.applied, vec!["local_address=10.0.9.2/32".to_string()]);

        let out = port_alias_set(PortAliasSetArgs {
            provider: "tunnel-test".into(),
            set: PortAliasSet {
                alias: "vpn_fwd".into(),
                ports: vec![51413],
            },
        })
        .await
        .expect("routes");
        assert!(out.changed);
        assert!(deregister_provider("tunnel-test"));
    }

    #[tokio::test]
    async fn unknown_owner_errors() {
        assert!(apply(sample_apply("nope-tunnel")).await.is_err());
        assert!(
            port_alias_set(PortAliasSetArgs {
                provider: "nope".into(),
                set: PortAliasSet {
                    alias: "a".into(),
                    ports: vec![],
                },
            })
            .await
            .is_err()
        );
    }
}
