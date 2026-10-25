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

// The erased-invoke boundary carries args/results/errors as `serde_json::Value`
// (the wire already produces a parsed value; no String hop). This module names
// that type in its `InvokeThunk`/proxy — the sanctioned opaque seam, scoped here.
#![allow(clippy::disallowed_types)]

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

    /// One-shot restore contract: `put` keeps exactly one previous value,
    /// `restore_previous` reinstates it and forgets it, so a second restore
    /// without an intervening `put` errors. Core also refuses that second call.
    async fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::read_only()
    }

    async fn put(&self, _ref_path: &str, _value: &str) -> Result<()> {
        anyhow::bail!("secrets backend '{}' does not support put", self.name())
    }

    async fn restore_previous(&self, _ref_path: &str) -> Result<()> {
        anyhow::bail!("secrets backend '{}' does not support restore", self.name())
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone, Debug, PartialEq, Eq,
)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Capability {
    Available,
    Unavailable { reason: String },
    Unsupported,
}

impl Capability {
    pub fn is_available(&self) -> bool {
        matches!(self, Capability::Available)
    }
}

#[derive(
    serde::Serialize, serde::Deserialize, schemars::JsonSchema, Clone, Debug, PartialEq, Eq,
)]
#[serde(rename_all = "camelCase")]
pub struct BackendCapabilities {
    pub resolve: Capability,
    pub put: Capability,
    pub restore: Capability,
}

impl BackendCapabilities {
    pub fn read_only() -> Self {
        Self {
            resolve: Capability::Available,
            put: Capability::Unsupported,
            restore: Capability::Unsupported,
        }
    }

    pub fn writable() -> Self {
        Self {
            resolve: Capability::Available,
            put: Capability::Available,
            restore: Capability::Available,
        }
    }

    pub fn write_unavailable(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            resolve: Capability::Available,
            put: Capability::Unavailable {
                reason: reason.clone(),
            },
            restore: Capability::Unavailable { reason },
        }
    }
}

type Registered = (Arc<dyn SecretsBackend>, String);

static GLOBAL: LazyLock<RwLock<Vec<Registered>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a secrets backend with the process-global registry. Names are
/// unique: a second registration under a held name is refused, so a reload
/// must deregister first.
pub fn register_provider(provider: Arc<dyn SecretsBackend>) -> Result<()> {
    let mut g = GLOBAL.write().expect("secrets_backend registry poisoned");
    let name = provider.name().to_string();
    if g.iter().any(|(p, _)| p.name() == name) {
        anyhow::bail!("secrets backend '{name}' is already registered");
    }
    g.push((provider, utils::id::new()));
    Ok(())
}

/// Snapshot of every registered backend.
pub fn providers() -> Vec<Arc<dyn SecretsBackend>> {
    GLOBAL
        .read()
        .expect("secrets_backend registry poisoned")
        .iter()
        .map(|(p, _)| p.clone())
        .collect()
}

/// Identifies one registration of `name`; a re-registration gets a new epoch.
pub fn provider_epoch(name: &str) -> Option<String> {
    GLOBAL
        .read()
        .expect("secrets_backend registry poisoned")
        .iter()
        .find(|(p, _)| p.name() == name)
        .map(|(_, e)| e.clone())
}

/// Deregister the backend named `name`, if present. The reversal path a plugin
/// unload needs. Returns `true` if a backend was removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("secrets_backend registry poisoned");
    let before = g.len();
    g.retain(|(p, _)| p.name() != name);
    before != g.len()
}

/// Resolve `ref_path` through the registered backend whose `name()` matches
/// `backend_kind`. Errors — never logging or including the resolved value — when
/// no such backend is registered on this host.
pub async fn resolve(backend_kind: &str, ref_path: &str) -> Result<String> {
    find(backend_kind)?.resolve(ref_path).await
}

fn find(backend_kind: &str) -> Result<Arc<dyn SecretsBackend>> {
    providers()
        .into_iter()
        .find(|p| p.name() == backend_kind)
        .ok_or_else(|| {
            anyhow::anyhow!("no secrets backend '{backend_kind}' is registered on this host")
        })
}

pub async fn capabilities(backend_kind: &str) -> Result<BackendCapabilities> {
    Ok(find(backend_kind)?.capabilities().await)
}

pub async fn put(backend_kind: &str, ref_path: &str, value: &str) -> Result<()> {
    let provider = find(backend_kind)?;
    match provider.capabilities().await.put {
        Capability::Available => provider.put(ref_path, value).await,
        Capability::Unavailable { reason } => {
            anyhow::bail!("secrets backend '{backend_kind}' write unavailable: {reason}")
        }
        Capability::Unsupported => {
            anyhow::bail!("secrets backend '{backend_kind}' does not support put")
        }
    }
}

pub async fn restore_previous(backend_kind: &str, ref_path: &str) -> Result<()> {
    let provider = find(backend_kind)?;
    match provider.capabilities().await.restore {
        Capability::Available => provider.restore_previous(ref_path).await,
        Capability::Unavailable { reason } => {
            anyhow::bail!("secrets backend '{backend_kind}' restore unavailable: {reason}")
        }
        Capability::Unsupported => {
            anyhow::bail!("secrets backend '{backend_kind}' does not support restore")
        }
    }
}

// ── Host-side proxy for loaded plugins ────────────────────────────────────────

/// The synchronous invoke thunk a loaded plugin's secrets backend is driven
/// through: `(op, args) -> Result<result, error>`, all as `serde_json::Value`.
/// Plain `Fn` of `Value`s so `contract` stays free of any ABI/loader dependency
/// (no cycle).
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
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

/// Operation name the [`SecretsBackendProxy`] invokes across the FFI boundary.
/// The plugin exposes a tool `"{invoke_prefix}.{RESOLVE_OP}"` taking
/// `{"ref_path": <string>}` and returning the raw secret value as a JSON string.
pub const RESOLVE_OP: &str = "resolve";

pub const CAPABILITIES_OP: &str = "capabilities";

pub const PUT_OP: &str = "put";

pub const RESTORE_OP: &str = "restore";

/// Plugin-side dispatch: answer the proxied `resolve` op by calling the typed
/// [`SecretsBackend`]. Args are `{"ref_path": <string>}`; the resolved secret is
/// returned JSON-string-encoded (the host decodes `from_value::<String>`).
pub async fn dispatch_op(
    backend: &dyn SecretsBackend,
    op: &str,
    args: serde_json::Value,
) -> std::result::Result<serde_json::Value, serde_json::Value> {
    fn err(msg: impl Into<String>) -> serde_json::Value {
        serde_json::Value::String(msg.into())
    }
    match op {
        RESOLVE_OP => {
            let ref_path = args
                .get("ref_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| err("resolve args missing string field 'ref_path'"))?;
            let value = backend
                .resolve(ref_path)
                .await
                .map_err(|e| err(format!("{e:#}")))?;
            serde_json::to_value(value).map_err(|e| err(e.to_string()))
        }
        CAPABILITIES_OP => {
            serde_json::to_value(backend.capabilities().await).map_err(|e| err(e.to_string()))
        }
        PUT_OP => {
            let ref_path = str_arg(&args, "ref_path")
                .ok_or_else(|| err("put args missing string field 'ref_path'"))?;
            let value = str_arg(&args, "value")
                .ok_or_else(|| err("put args missing string field 'value'"))?;
            backend
                .put(ref_path, value)
                .await
                .map(|()| serde_json::Value::Null)
                .map_err(|e| err(format!("{e:#}")))
        }
        RESTORE_OP => {
            let ref_path = str_arg(&args, "ref_path")
                .ok_or_else(|| err("restore args missing string field 'ref_path'"))?;
            backend
                .restore_previous(ref_path)
                .await
                .map(|()| serde_json::Value::Null)
                .map_err(|e| err(format!("{e:#}")))
        }
        other => Err(err(format!("secrets backend has no operation '{other}'"))),
    }
}

fn str_arg<'a>(args: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(|v| v.as_str())
}

/// Build and register a [`SecretsBackend`] from a plugin backend descriptor plus
/// an [`InvokeThunk`]. The plugin-loader calls this from its domain dispatch
/// table for `domain = "secrets_backend"`.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(SecretsBackendProxy { name, invoke }))
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
        let out = self
            .call(RESOLVE_OP, serde_json::json!({ "ref_path": ref_path }))
            .await?;
        let value: String = serde_json::from_value(out).map_err(|e| {
            anyhow::anyhow!("secrets_backend '{}' returned invalid JSON: {e}", self.name)
        })?;
        Ok(value)
    }

    async fn capabilities(&self) -> BackendCapabilities {
        match self.call(CAPABILITIES_OP, serde_json::json!({})).await {
            Ok(out) => serde_json::from_value(out).unwrap_or_else(|e| {
                BackendCapabilities::write_unavailable(format!(
                    "invalid capabilities response: {e}"
                ))
            }),
            Err(e) => BackendCapabilities::write_unavailable(format!("{e:#}")),
        }
    }

    async fn put(&self, ref_path: &str, value: &str) -> Result<()> {
        self.call(
            PUT_OP,
            serde_json::json!({ "ref_path": ref_path, "value": value }),
        )
        .await
        .map(drop)
        .map_err(|e| anyhow::anyhow!(utils::scrub::redact_values(&format!("{e:#}"), &[value])))
    }

    async fn restore_previous(&self, ref_path: &str) -> Result<()> {
        self.call(RESTORE_OP, serde_json::json!({ "ref_path": ref_path }))
            .await
            .map(drop)
            .map_err(|e| anyhow::anyhow!(utils::scrub::redact_values(&format!("{e:#}"), &[])))
    }
}

#[cfg(feature = "in-process")]
impl SecretsBackendProxy {
    async fn call(&self, op: &'static str, args: serde_json::Value) -> Result<serde_json::Value> {
        let invoke = self.invoke.clone();
        let name = self.name.clone();
        let call = crate::invocation_scope::capture("secrets_backend", &name);
        tokio::task::spawn_blocking(move || invoke(op, args, call))
            .await
            .map_err(|e| anyhow::anyhow!("secrets_backend '{name}' invoke task panicked: {e}"))?
            .map_err(|e| {
                anyhow::anyhow!(
                    "secrets_backend '{}' {op} failed: {}",
                    self.name,
                    crate::render_invoke_error(&e)
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        name: &'static str,
        caps: BackendCapabilities,
        value: Mutex<(String, Option<String>)>,
    }

    impl Fake {
        fn new(name: &'static str, caps: BackendCapabilities) -> Arc<Self> {
            Arc::new(Self {
                name,
                caps,
                value: Mutex::new(("old".into(), None)),
            })
        }
    }

    #[async_trait::async_trait]
    impl SecretsBackend for Fake {
        fn name(&self) -> &str {
            self.name
        }
        async fn resolve(&self, _ref_path: &str) -> Result<String> {
            Ok(self.value.lock().unwrap().0.clone())
        }
        async fn capabilities(&self) -> BackendCapabilities {
            self.caps.clone()
        }
        async fn put(&self, _ref_path: &str, value: &str) -> Result<()> {
            let mut v = self.value.lock().unwrap();
            let prev = std::mem::replace(&mut v.0, value.to_string());
            v.1 = Some(prev);
            Ok(())
        }
        async fn restore_previous(&self, _ref_path: &str) -> Result<()> {
            let mut v = self.value.lock().unwrap();
            let prev = v.1.take().ok_or_else(|| anyhow::anyhow!("no previous"))?;
            v.0 = prev;
            Ok(())
        }
    }

    struct ReadOnly;

    #[async_trait::async_trait]
    impl SecretsBackend for ReadOnly {
        fn name(&self) -> &str {
            "contract-test-readonly"
        }
        async fn resolve(&self, _ref_path: &str) -> Result<String> {
            Ok("v".into())
        }
    }

    #[tokio::test]
    async fn put_then_restore_round_trips_through_the_registry() {
        let kind = "contract-test-writable";
        register_provider(Fake::new(kind, BackendCapabilities::writable())).unwrap();
        put(kind, "r", "new").await.unwrap();
        assert_eq!(resolve(kind, "r").await.unwrap(), "new");
        restore_previous(kind, "r").await.unwrap();
        assert_eq!(resolve(kind, "r").await.unwrap(), "old");
        assert!(deregister_provider(kind));
    }

    #[tokio::test]
    async fn put_is_refused_when_write_is_unavailable() {
        let kind = "contract-test-unavailable";
        let fake = Fake::new(
            kind,
            BackendCapabilities::write_unavailable("read-only token"),
        );
        register_provider(fake.clone()).unwrap();
        let err = put(kind, "r", "new").await.unwrap_err().to_string();
        assert!(err.contains("write unavailable: read-only token"), "{err}");
        assert_eq!(fake.value.lock().unwrap().0, "old");
        assert!(restore_previous(kind, "r").await.is_err());
        assert!(deregister_provider(kind));
    }

    #[test]
    fn duplicate_backend_name_is_refused() {
        let kind = "contract-test-dup";
        register_provider(Fake::new(kind, BackendCapabilities::writable())).unwrap();
        let err = register_provider(Fake::new(kind, BackendCapabilities::read_only()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already registered"), "{err}");
        let first = provider_epoch(kind).unwrap();
        assert!(deregister_provider(kind));
        assert_eq!(provider_epoch(kind), None);
        register_provider(Fake::new(kind, BackendCapabilities::read_only())).unwrap();
        assert_ne!(provider_epoch(kind).unwrap(), first);
        assert!(deregister_provider(kind));
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn proxy_put_error_never_echoes_the_value() {
        let proxy = SecretsBackendProxy {
            name: "contract-test-scrub".into(),
            invoke: Arc::new(|_op, args, _call| {
                Err(serde_json::json!(format!("rejected {}", args["value"])))
            }),
        };
        let err = proxy
            .put("r", "hunter2-secret")
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("hunter2-secret"), "{err}");
        assert!(err.contains(utils::scrub::REDACTED_TEXT), "{err}");
    }

    #[tokio::test]
    async fn default_backend_is_read_only() {
        register_provider(Arc::new(ReadOnly)).unwrap();
        let caps = capabilities("contract-test-readonly").await.unwrap();
        assert_eq!(caps, BackendCapabilities::read_only());
        let err = put("contract-test-readonly", "r", "x").await.unwrap_err();
        assert!(err.to_string().contains("does not support put"), "{err}");
        assert!(deregister_provider("contract-test-readonly"));
    }

    #[tokio::test]
    async fn dispatch_op_serves_write_ops() {
        let fake = Fake::new("contract-test-dispatch", BackendCapabilities::writable());
        let caps = dispatch_op(fake.as_ref(), CAPABILITIES_OP, serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_value::<BackendCapabilities>(caps).unwrap(),
            BackendCapabilities::writable()
        );
        dispatch_op(
            fake.as_ref(),
            PUT_OP,
            serde_json::json!({ "ref_path": "r", "value": "new" }),
        )
        .await
        .unwrap();
        assert_eq!(fake.value.lock().unwrap().0, "new");
        dispatch_op(
            fake.as_ref(),
            RESTORE_OP,
            serde_json::json!({ "ref_path": "r" }),
        )
        .await
        .unwrap();
        assert_eq!(fake.value.lock().unwrap().0, "old");
        assert!(
            dispatch_op(
                fake.as_ref(),
                PUT_OP,
                serde_json::json!({ "ref_path": "r" })
            )
            .await
            .is_err()
        );
    }

    #[cfg(feature = "in-process")]
    #[tokio::test]
    async fn proxy_without_write_ops_reports_unavailable_with_reason() {
        let invoke: InvokeThunk = Arc::new(|op, _args, _call| match op {
            RESOLVE_OP => Ok(serde_json::json!("v")),
            other => Err(serde_json::json!(format!("no tool '{other}'"))),
        });
        let proxy = SecretsBackendProxy {
            name: "contract-test-proxy".into(),
            invoke,
        };
        let caps = proxy.capabilities().await;
        assert_eq!(caps.resolve, Capability::Available);
        match &caps.put {
            Capability::Unavailable { reason } => {
                assert!(reason.contains("no tool 'capabilities'"), "{reason}")
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        assert!(matches!(caps.restore, Capability::Unavailable { .. }));

        let bad: InvokeThunk = Arc::new(|_op, _args, _call| Ok(serde_json::json!(42)));
        let proxy = SecretsBackendProxy {
            name: "contract-test-proxy-bad".into(),
            invoke: bad,
        };
        match proxy.capabilities().await.put {
            Capability::Unavailable { reason } => {
                assert!(reason.contains("invalid capabilities response"), "{reason}")
            }
            other => panic!("expected unavailable, got {other:?}"),
        }

        register_from_def(
            "contract-test-proxy-reg".into(),
            Arc::new(|op, _a, _c| Err(serde_json::json!(format!("no tool '{op}'")))),
        )
        .unwrap();
        let err = put("contract-test-proxy-reg", "r", "x").await.unwrap_err();
        assert!(err.to_string().contains("write unavailable"), "{err}");
        assert!(deregister_provider("contract-test-proxy-reg"));
    }

    #[test]
    fn capability_serializes_with_a_state_tag() {
        let v = serde_json::to_value(BackendCapabilities::write_unavailable("ro")).unwrap();
        assert_eq!(v["put"]["state"], "unavailable");
        assert_eq!(v["put"]["reason"], "ro");
        assert_eq!(v["resolve"]["state"], "available");
    }
}
