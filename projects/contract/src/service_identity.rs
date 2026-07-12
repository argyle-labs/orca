//! Runtime service-identity registration domain.
//!
//! A colocated plugin (sonarr, adguard, qbittorrent, …) registers a
//! [`ServiceRegistration`] bound to a network endpoint (host + port) at
//! runtime, declaring *what type of service it is* and the typed
//! [`ServicePrimitive`]s it exposes from the core. The inventory layer
//! correlates each registration to a running [`crate::topology::TopologyClaim`]
//! by `(host, port)`, so a container node learns its service role and endpoint
//! link without core hardcoding any role table (roles are free strings the
//! plugin owns) and without image-name guessing.
//!
//! ## Registry + aggregator
//!
//! Providers register into a process-global registry ([`register_backend`] /
//! [`register_from_def`]). [`collect_registrations`] fans `list_registrations()`
//! out across every registered provider and concatenates, so the correlation
//! pass sees one flat set regardless of how many plugins contribute. This
//! mirrors the `topology` / `cluster_roster` domain registries the
//! plugin-loader already drives; async trait methods are hand-desugared to
//! [`BoxFuture`] (no `async_trait` macro, per the workspace rule).

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// A typed capability a service exposes from the core, declared at
/// registration. "Primitives from core" — the generic building blocks core
/// owns, never opaque JSON.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServicePrimitive {
    /// A reachable HTTP(S) API surface. `path` e.g. `"/api/v3"`.
    HttpApi { path: String },
    /// A webhook/callback the service can emit.
    Webhook { event: String },
    /// A metrics endpoint (prometheus etc). `path` e.g. `"/metrics"`.
    Metrics { path: String },
    /// A health/readiness probe. `path` e.g. `"/ping"`.
    Health { path: String },
}

/// One service identity a plugin registers at runtime, bound to an endpoint.
///
/// This is a wire + FFI shape (crosses the JSON proxy and, later, gossip) — keep
/// it additive: every non-essential field is `#[serde(default, …)]` so an older
/// peer's payload still deserializes.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ServiceRegistration {
    /// Logical role/type, e.g. `"sonarr"`, `"adguard"`. A free string owned by
    /// the plugin; core does not enumerate roles.
    pub role: String,
    /// Host or IP the service is reached at. Correlated against a peer's
    /// hostname and every known network address, the same resolution
    /// [`crate::topology::TopologyClaim::runs_on`] gets.
    pub host: String,
    /// Port the service listens on — the join key against a claim's endpoints.
    pub port: u16,
    /// Provider that registered this (registry key component + display).
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primitives: Vec<ServicePrimitive>,
}

// ── Provider registry ────────────────────────────────────────────────────────

/// A source of [`ServiceRegistration`]s — one per plugin. Registered into the
/// process-global registry so [`collect_registrations`] can fan out
/// plugin-agnostically. Async methods return [`BoxFuture`].
pub trait ServiceIdentityProvider: Send + Sync {
    /// Provider/registry name (e.g. `"sonarr"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    fn list_registrations(&self) -> BoxFuture<'_, Result<Vec<ServiceRegistration>>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn ServiceIdentityProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a service-identity provider with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate providers.
pub fn register_backend(backend: Arc<dyn ServiceIdentityProvider>) {
    let mut g = GLOBAL.write().expect("service_identity registry poisoned");
    let name = backend.name().to_string();
    if let Some(slot) = g.iter_mut().find(|b| b.name() == name) {
        *slot = backend;
    } else {
        g.push(backend);
    }
}

/// Snapshot of every registered provider.
pub fn backends() -> Vec<Arc<dyn ServiceIdentityProvider>> {
    GLOBAL
        .read()
        .expect("service_identity registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. The reversal path a plugin
/// unload needs so a dropped plugin leaves no provider pointing at a dead invoke
/// thunk. Returns `true` if a provider was removed.
pub fn deregister_backend(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("service_identity registry poisoned");
    let before = g.len();
    g.retain(|b| b.name() != name);
    before != g.len()
}

/// Fan `list_registrations()` out across every registered provider and
/// concatenate. A provider that errors is logged and skipped — one broken
/// plugin must not blank out the whole set.
pub async fn collect_registrations() -> Vec<ServiceRegistration> {
    let mut out = Vec::new();
    for backend in backends() {
        match backend.list_registrations().await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(
                provider = %backend.name(),
                error = %e,
                "service_identity provider failed",
            ),
        }
    }
    out
}

// ── FFI proxy ─────────────────────────────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's provider is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of strings
/// so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`ServiceIdentityProxy`] invokes across the FFI boundary.
/// The plugin exposes a tool `"{invoke_prefix}.{LIST_OP}"` returning a JSON
/// `Vec<ServiceRegistration>`.
pub const LIST_OP: &str = "list_registrations";

/// Build and register a [`ServiceIdentityProvider`] from a plugin backend
/// descriptor plus an [`InvokeThunk`]. The plugin-loader calls this from
/// its domain dispatch table for `domain = "service_identity"`. Registration
/// replaces any existing provider of the same name (idempotent reload).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_backend(Arc::new(ServiceIdentityProxy { name, invoke }));
    Ok(())
}

/// A [`ServiceIdentityProvider`] backed by a subprocess plugin reached over the
/// JSON-proxy FFI boundary. `list_registrations()` offloads the synchronous
/// [`InvokeThunk`] onto `spawn_blocking` (so a slow/wedged plugin never blocks
/// the async runtime) and deserializes the JSON result.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct ServiceIdentityProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl ServiceIdentityProvider for ServiceIdentityProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_registrations(&self) -> BoxFuture<'_, Result<Vec<ServiceRegistration>>> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        Box::pin(async move {
            let out = tokio::task::spawn_blocking(move || invoke(LIST_OP, "{}".to_string()))
                .await
                .map_err(|e| {
                    anyhow::anyhow!("service_identity '{name}' invoke task panicked: {e}")
                })?
                .map_err(|e| anyhow::anyhow!("service_identity '{name}' invoke failed: {e}"))?;
            serde_json::from_str(&out).map_err(|e| {
                anyhow::anyhow!("service_identity '{name}' returned invalid JSON: {e}")
            })
        })
    }
}
