//! Secrets-backend: a plugin that resolves secret references to values.
//!
//! Core stores a secret's metadata (name, backend kind, `ref_path`) but must
//! not embed the platform knowledge to turn a reference like
//! `op://Personal/orca-gh/token` into a value — that lives in a backend plugin
//! (1Password, Bitwarden, Vault, …). A colocated provider contributes
//! resolution through a [`SecretsBackend`] registered into a process-global
//! registry — either in-process or, for an external subprocess plugin, a
//! [`register_from_def`] JSON proxy the plugin-loader installs for
//! `domain = "secrets_backend"`. The `auth` crate's `get_secret` resolves a
//! non-inline secret by dispatching to the provider whose `name()` matches the
//! secret's backend kind, the same way the `host_facts` domain works.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;

// ── Provider registry ────────────────────────────────────────────────────────

/// A backend that resolves a secret reference to its value — one per backend
/// kind. Registered into the process-global registry so `auth` can dispatch to
/// it plugin-agnostically.
#[async_trait::async_trait]
pub trait SecretsBackend: Send + Sync {
    /// Backend KIND (e.g. `"onepassword"`). Registry key; used to replace-in-place
    /// on re-register, to deregister on plugin unload, and to match a secret's
    /// recorded backend.
    fn name(&self) -> &str;

    async fn resolve(&self, ref_path: &str) -> Result<String>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn SecretsBackend>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a secrets backend with the process-global registry.
/// Re-registering the same `name()` replaces the existing entry so a dev
/// rebuild / plugin reload doesn't duplicate backends.
pub fn register_provider(provider: Arc<dyn SecretsBackend>) {
    let mut g = GLOBAL.write().expect("secrets_backend registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered backend.
pub fn providers() -> Vec<Arc<dyn SecretsBackend>> {
    GLOBAL
        .read()
        .expect("secrets_backend registry poisoned")
        .clone()
}

/// Deregister the backend named `name`, if present. The reversal path a plugin
/// unload needs. Returns `true` if a backend was removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("secrets_backend registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Resolve `ref_path` through the registered backend whose `name()` matches
/// `backend_kind`. Errors — never logging or including the resolved value — when
/// no such backend is registered on this host.
pub async fn resolve(backend_kind: &str, ref_path: &str) -> Result<String> {
    let provider = providers()
        .into_iter()
        .find(|p| p.name() == backend_kind)
        .ok_or_else(|| {
            anyhow::anyhow!("no secrets backend '{backend_kind}' is registered on this host")
        })?;
    provider.resolve(ref_path).await
}

// ── Host-side proxy for loaded plugins ────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's secrets backend is driven
/// through: `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn`
/// of strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation name the [`SecretsBackendProxy`] invokes across the FFI boundary.
/// The plugin exposes a tool `"{invoke_prefix}.{RESOLVE_OP}"` taking
/// `{"ref_path": <string>}` and returning the raw secret value as a JSON string.
pub const RESOLVE_OP: &str = "resolve";

/// Build and register a [`SecretsBackend`] from a plugin backend descriptor plus
/// an [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch
/// table for `domain = "secrets_backend"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(SecretsBackendProxy { name, invoke }));
    Ok(())
}

/// A [`SecretsBackend`] backed by a subprocess plugin reached over the JSON-proxy
/// FFI boundary. `resolve()` offloads the synchronous [`InvokeThunk`] onto
/// `spawn_blocking` and deserializes the JSON string result.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct SecretsBackendProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
#[async_trait::async_trait]
impl SecretsBackend for SecretsBackendProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn resolve(&self, ref_path: &str) -> Result<String> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let args = serde_json::json!({ "ref_path": ref_path }).to_string();
        let out = tokio::task::spawn_blocking(move || invoke(RESOLVE_OP, args))
            .await
            .map_err(|e| anyhow::anyhow!("secrets_backend '{name}' invoke task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("secrets_backend '{name}' invoke failed: {e}"))?;
        let value: String = serde_json::from_str(&out)
            .map_err(|e| anyhow::anyhow!("secrets_backend '{name}' returned invalid JSON: {e}"))?;
        Ok(value)
    }
}
