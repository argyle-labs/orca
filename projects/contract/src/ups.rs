//! Cross-crate UPS / power capability types and provider registry.
//!
//! A UPS provider inspects and configures a host's uninterruptible power supply.
//! It is the *same universal pattern* as [`crate::diagnostics`] / [`crate::topology`]:
//! core owns the primitives and the surface; plugins implement them as providers.
//! **NUT is one provider** (standalone `upsd`/`upsmon`); **unraid is another**
//! (native apcupsd, driven over the Unraid GraphQL API). We have more systems than
//! Unraid — each system's native UPS stack is a provider, no forced migration.
//! Both surface the same `ups.*` capability, reached through different pathways.
//!
//! The surface layer (`dispatch::ups_surface`) fans `state`/`config_get` across
//! every provider and routes `config_set` to the owning one, so the surface stays
//! plugin-agnostic. Shutdown ordering (later) is built on these primitives plus
//! the topology graph.
//!
//! ## Provider registry
//!
//! A provider registers into a process-global registry — either in-process or,
//! for an external subprocess plugin, via the [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "ups"`.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// Live state of one UPS, as read from a provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct UpsState {
    /// Provider that reported this UPS (registry key, e.g. `"nut"` / `"unraid"`).
    pub provider: String,
    /// Stable id of the UPS within its provider (e.g. the NUT `ups` name or the
    /// Unraid device id). A host with one UPS commonly uses `"default"`.
    pub id: String,
    /// Model/description string when the provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Battery charge percent (0–100) when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_charge: Option<f64>,
    /// Estimated battery runtime remaining, in **milliseconds**, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_runtime_ms: Option<i64>,
    /// Input line voltage when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_voltage: Option<f64>,
    /// UPS load percent (0–100) when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_percent: Option<f64>,
    /// Raw status flags as the provider reports them (e.g. NUT `"OL"`, `"OB LB"`).
    pub status: String,
    /// Running on battery (mains lost). Derived from `status` by the provider.
    pub on_battery: bool,
    /// Battery low — shutdown is imminent. Derived from `status` by the provider.
    pub low_battery: bool,
}

/// Power/shutdown configuration of one UPS. A superset covering both NUT
/// (`upsmon`) and Unraid apcupsd (`configureUps`) knobs; a provider fills the
/// fields it supports and ignores the rest.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct UpsConfig {
    /// UPS id this config applies to (matches [`UpsState::id`]).
    pub id: String,
    /// Battery-charge percent threshold that triggers shutdown (apcupsd
    /// `BATTERYLEVEL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_level: Option<i64>,
    /// Runtime-remaining threshold that triggers shutdown, in **milliseconds**
    /// (apcupsd `MINUTES`, converted at the edge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_runtime_ms: Option<i64>,
    /// Time on battery before forcing shutdown regardless of charge, in
    /// **milliseconds** (apcupsd `TIMEOUT`; `0` = disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_battery_timeout_ms: Option<i64>,
    /// Cut UPS power after the OS halts, so the UPS actually powers off and
    /// re-powers when mains return (apcupsd `KILLPOWER` / NUT `killpower`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_power: Option<bool>,
    /// The command run to shut this host down on a critical UPS event (NUT
    /// `SHUTDOWNCMD`). Providers that manage this expose it; others leave `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_cmd: Option<String>,
}

/// Result of a [`UpsProvider::config_set`].
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct UpsConfigOutcome {
    /// UPS id the config was applied to.
    pub id: String,
    /// Provider that applied it.
    pub provider: String,
    /// Whether the change applied successfully.
    pub ok: bool,
    /// Human-readable result (what changed, or why it couldn't be).
    pub message: String,
    /// The change needs a service/host restart to take effect.
    #[serde(default)]
    pub restart_required: bool,
}

/// Filter for a `state` / `config_get` fan-out. Empty = every provider + UPS.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct UpsQueryArgs {
    /// Restrict to a single provider by registry name. `None` = all providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Restrict to a single UPS id within the provider(s). `None` = all UPSes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Arguments to apply a UPS config, routed to the named provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct UpsConfigSetArgs {
    /// Provider registry name that owns the UPS.
    pub provider: String,
    /// The configuration to apply (only set fields are changed).
    pub config: UpsConfig,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A UPS state/config source — one per provider (nut, unraid, …). Registered
/// into the process-global registry so the surface layer can fan out across
/// providers plugin-agnostically.
pub trait UpsProvider: Send + Sync {
    /// Provider/registry name (e.g. `"nut"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    /// Read live state of the UPS(es) this provider knows (honouring the filter).
    fn state(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsState>>>;

    /// Read the power/shutdown configuration of the UPS(es).
    fn config_get(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsConfig>>>;

    /// Apply a configuration change to one UPS.
    fn config_set(&self, config: UpsConfig) -> BoxFuture<'_, Result<UpsConfigOutcome>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn UpsProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a UPS provider. Re-registering the same `name()` replaces the
/// existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn UpsProvider>) {
    let mut g = GLOBAL.write().expect("ups registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn UpsProvider>> {
    GLOBAL.read().expect("ups registry poisoned").clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("ups registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points (used by dispatch::ups_surface) ─────────────────────

/// Fan `state` across all providers (or one, if `args.provider` is set),
/// concatenating results. A provider that errors is skipped — one broken
/// provider must not blank the whole report.
pub async fn state(args: UpsQueryArgs) -> Vec<UpsState> {
    let mut out = Vec::new();
    for p in providers() {
        if let Some(want) = &args.provider
            && p.name() != want
        {
            continue;
        }
        if let Ok(mut s) = p.state(args.clone()).await {
            out.append(&mut s);
        }
    }
    out
}

/// Fan `config_get` across all providers (or one). Skips errored providers.
pub async fn config_get(args: UpsQueryArgs) -> Vec<UpsConfig> {
    let mut out = Vec::new();
    for p in providers() {
        if let Some(want) = &args.provider
            && p.name() != want
        {
            continue;
        }
        if let Ok(mut c) = p.config_get(args.clone()).await {
            out.append(&mut c);
        }
    }
    out
}

/// Route one `config_set` to the provider that owns it.
pub async fn config_set(args: UpsConfigSetArgs) -> Result<UpsConfigOutcome> {
    let provider = providers()
        .into_iter()
        .find(|p| p.name() == args.provider)
        .ok_or_else(|| anyhow::anyhow!("no ups provider named '{}'", args.provider))?;
    provider.config_set(args.config).await
}

// ── Host-side loaded-plugin proxy ─────────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's provider is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of
/// strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation names the [`UpsProxy`] invokes across the FFI boundary. The plugin
/// exposes tools `"{invoke_prefix}.{STATE_OP|CONFIG_GET_OP|CONFIG_SET_OP}"`.
pub const STATE_OP: &str = "state";
pub const CONFIG_GET_OP: &str = "config_get";
pub const CONFIG_SET_OP: &str = "config_set";

/// Build and register a [`UpsProvider`] from a plugin backend descriptor plus an
/// [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch table
/// for `domain = "ups"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(UpsProxy { name, invoke }));
    Ok(())
}

/// A [`UpsProvider`] backed by a subprocess plugin reached over the JSON-proxy
/// FFI boundary. Each op offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and (de)serializes JSON at the seam.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct UpsProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl UpsProxy {
    fn call<T: for<'de> Deserialize<'de>>(
        &self,
        op: &'static str,
        args_json: String,
    ) -> BoxFuture<'_, Result<T>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(op, args_json))
                .await
                .map_err(|e| anyhow::anyhow!("ups '{name}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("ups '{name}' {op} failed: {e}"))?;
            serde_json::from_str(&out)
                .map_err(|e| anyhow::anyhow!("ups '{name}' {op} returned invalid JSON: {e}"))
        })
    }
}

#[cfg(feature = "in-process")]
impl UpsProvider for UpsProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsState>>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(STATE_OP, args_json)
    }

    fn config_get(&self, args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsConfig>>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(CONFIG_GET_OP, args_json)
    }

    fn config_set(&self, config: UpsConfig) -> BoxFuture<'_, Result<UpsConfigOutcome>> {
        let args_json = serde_json::to_string(&config).unwrap_or_else(|_| "{}".to_string());
        self.call(CONFIG_SET_OP, args_json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeUps {
        name: String,
    }

    impl UpsProvider for FakeUps {
        fn name(&self) -> &str {
            &self.name
        }
        fn state(&self, _args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsState>>> {
            let provider = self.name.clone();
            Box::pin(async move {
                Ok(vec![UpsState {
                    provider,
                    id: "default".into(),
                    model: None,
                    battery_charge: Some(100.0),
                    battery_runtime_ms: Some(1_800_000),
                    input_voltage: Some(120.0),
                    load_percent: Some(12.0),
                    status: "OL".into(),
                    on_battery: false,
                    low_battery: false,
                }])
            })
        }
        fn config_get(&self, _args: UpsQueryArgs) -> BoxFuture<'_, Result<Vec<UpsConfig>>> {
            Box::pin(async move {
                Ok(vec![UpsConfig {
                    id: "default".into(),
                    kill_power: Some(true),
                    ..Default::default()
                }])
            })
        }
        fn config_set(&self, config: UpsConfig) -> BoxFuture<'_, Result<UpsConfigOutcome>> {
            let provider = self.name.clone();
            Box::pin(async move {
                Ok(UpsConfigOutcome {
                    id: config.id,
                    provider,
                    ok: true,
                    message: "applied".into(),
                    restart_required: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn fan_out_and_route() {
        register_provider(Arc::new(FakeUps {
            name: "ups-test".into(),
        }));
        let s = state(UpsQueryArgs {
            provider: Some("ups-test".into()),
            id: None,
        })
        .await;
        assert!(s.iter().any(|u| u.provider == "ups-test" && !u.on_battery));

        let out = config_set(UpsConfigSetArgs {
            provider: "ups-test".into(),
            config: UpsConfig {
                id: "default".into(),
                kill_power: Some(true),
                ..Default::default()
            },
        })
        .await
        .expect("routes");
        assert!(out.ok);
        assert!(deregister_provider("ups-test"));
    }

    #[tokio::test]
    async fn config_set_unknown_provider_errors() {
        assert!(
            config_set(UpsConfigSetArgs {
                provider: "nope".into(),
                config: UpsConfig::default(),
            })
            .await
            .is_err()
        );
    }
}
