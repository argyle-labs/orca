//! Observed replication health — the *read* side of a replication relationship.
//!
//! A replication relationship (core's `storage.replication` generic) is split
//! per [[on-demand-not-poll-and-cache]] into **config** (what *should* be kept in
//! sync — provider/folder/member routes) and **observed health** (is it
//! *actually* synced). This module owns the observed side: a
//! [`ReplicationStatus`] and the provider seam that yields it, mirroring the
//! [`StorageBackend`](crate::StorageBackend) registry exactly — a process-global
//! set of registered providers, one per backend, that the plugin loader
//! populates from a plugin's descriptor (slice 4: the `syncthing` plugin).
//!
//! Core ships **no** provider. With none registered, [`resolve`] returns `None`
//! (status *Unknown*), and the converge failover-safety gate treats Unknown as
//! "hold" — failing an active mount over to a member whose replication is
//! unconfirmed is worse than waiting.

use crate::StorageError;
use derive::orca_async;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, RwLock};

/// Observed sync health of one replication relationship, measured host-local and
/// on-demand (never a poll-cache). The failover-safety gate consults `healthy`;
/// `last_sync_ms` / `detail` are for surfacing ("willow↔maple 100%, synced 2m
/// ago").
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationStatus {
    /// Backend that produced this reading (`syncthing`).
    pub provider: String,
    /// Whether every member of the relationship is currently in sync — the one
    /// signal the failover gate requires before permitting an active-route swap.
    pub healthy: bool,
    /// Epoch-millis of the last confirmed sync across members, when the provider
    /// tracks it ([[time-values-in-milliseconds]]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_ms: Option<i64>,
    /// Human-readable state for display / diagnostics, when the provider offers
    /// one. Never secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A replication-status provider adapter. The `syncthing` plugin implements one;
/// the loader registers it from the plugin's descriptor, exactly as
/// [`register_from_def`](crate::register_from_def) does for a
/// [`StorageBackend`](crate::StorageBackend). Default trait shape mirrors
/// `StorageBackend`: a `name` the registry dedupes on plus the observation call.
#[orca_async]
pub trait ReplicationStatusProvider: Send + Sync {
    /// Provider name the registry keys/dedupes on — the same string a
    /// relationship's `provider` field carries (`syncthing`).
    fn name(&self) -> &str;

    /// Observe the relationship's sync health, host-local and on-demand. `folder`
    /// is the provider's opaque folder id; `members` are the member host
    /// identifiers (a relationship's route `value`s). An error is surfaced by
    /// [`resolve`] as an *unhealthy* status carrying the error detail — the gate
    /// then holds, never fails over on an observation error.
    async fn status(
        &self,
        folder: &str,
        members: &[String],
    ) -> Result<ReplicationStatus, StorageError>;
}

// ── Process-global registry (mirrors the `StorageBackend` GLOBAL) ────────────

static PROVIDERS: LazyLock<RwLock<Vec<Arc<dyn ReplicationStatusProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a replication-status provider. Re-registering the same name replaces
/// the existing entry so a plugin reload doesn't duplicate providers — identical
/// semantics to [`register_backend`](crate::register_backend).
pub fn register_status_provider(provider: Arc<dyn ReplicationStatusProvider>) {
    let mut g = PROVIDERS
        .write()
        .expect("replication status registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered status provider.
pub fn status_providers() -> Vec<Arc<dyn ReplicationStatusProvider>> {
    PROVIDERS
        .read()
        .expect("replication status registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if one was
/// removed — the reversibility a plugin unload needs.
pub fn deregister_status_provider(name: &str) -> bool {
    let mut g = PROVIDERS
        .write()
        .expect("replication status registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

/// Observe a relationship's health via its registered provider.
///
/// - No provider registered for `provider` → `None` (status *Unknown*). The gate
///   treats this as "hold" — the state all of core is in until the `syncthing`
///   plugin registers its provider (slice 4).
/// - Provider present, observation ok → `Some(status)`.
/// - Provider present, observation errored → `Some` with `healthy = false` and
///   the error in `detail`, so the gate holds and the failure is surfaced rather
///   than silently read as healthy.
pub async fn resolve(
    provider: &str,
    folder: &str,
    members: &[String],
) -> Option<ReplicationStatus> {
    let p = status_providers()
        .into_iter()
        .find(|p| p.name() == provider)?;
    match p.status(folder, members).await {
        Ok(status) => Some(status),
        Err(e) => Some(ReplicationStatus {
            provider: provider.to_string(),
            healthy: false,
            last_sync_ms: None,
            detail: Some(e.to_string()),
        }),
    }
}

// ── Plugin-side dispatch (the wire's single source of truth) ──────────────────

/// Bare op name the [`ReplicationStatusProxy`] invokes across the FFI boundary.
/// The plugin exposes it as `"{invoke_prefix}.{STATUS_OP}"`, taking a JSON
/// [`StatusArgs`] and returning a JSON [`ReplicationStatus`].
pub const STATUS_OP: &str = "status";

/// Wire args for [`STATUS_OP`] — the [`ReplicationStatusProvider::status`]
/// parameters, encoded once here so both halves of the boundary agree.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StatusArgs {
    folder: String,
    members: Vec<String>,
}

/// Plugin-side inverse of [`ReplicationStatusProxy`]: decode a proxied op's JSON
/// args and route it to an in-process [`ReplicationStatusProvider`], returning
/// the op's JSON-encoded result (or an error string). A backend plugin's
/// `invoke` is one call to this function — never a hand-copied per-op `match`
/// that drifts from the proxy. `op` is the bare operation name (the loader's
/// thunk strips the invoke prefix first). Tokio-free — plugin side.
#[allow(clippy::disallowed_types)] // erased-invoke dispatch seam — Value in/out.
pub async fn dispatch_op(
    provider: &dyn ReplicationStatusProvider,
    op: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    fn err<E: std::fmt::Display>(e: E) -> serde_json::Value {
        serde_json::Value::String(e.to_string())
    }
    match op {
        STATUS_OP => {
            let a: StatusArgs = serde_json::from_value(args)
                .map_err(|e| err(format!("invalid `{STATUS_OP}` args: {e}")))?;
            let status = provider.status(&a.folder, &a.members).await.map_err(err)?;
            serde_json::to_value(&status).map_err(|e| err(format!("failed to encode result: {e}")))
        }
        other => Err(serde_json::Value::String(format!(
            "replication provider has no operation '{other}'"
        ))),
    }
}

// ── Host-side proxy for loaded plugins ────────────────────────────────────────

/// Build and register a [`ReplicationStatusProvider`] from a plugin's backend
/// descriptor plus an [`InvokeThunk`](crate::InvokeThunk). The loader calls this
/// from its domain dispatch table for `domain = "replication"`. Registration
/// replaces any existing provider of the same name (idempotent reload), matching
/// [`register_status_provider`]'s semantics.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: crate::InvokeThunk) -> Result<(), StorageError> {
    register_status_provider(Arc::new(ReplicationStatusProxy { name, invoke }));
    Ok(())
}

/// A [`ReplicationStatusProvider`] backed by a subprocess plugin reached over the
/// JSON-proxy wire. `status()` serializes its args, offloads the synchronous
/// [`InvokeThunk`](crate::InvokeThunk) onto `spawn_blocking` (so a slow/wedged
/// plugin never blocks the async runtime), and deserializes the JSON result.
///
/// Host-side loaded-plugin proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct ReplicationStatusProxy {
    name: String,
    invoke: crate::InvokeThunk,
}

#[cfg(feature = "in-process")]
#[orca_async]
impl ReplicationStatusProvider for ReplicationStatusProxy {
    fn name(&self) -> &str {
        &self.name
    }

    async fn status(
        &self,
        folder: &str,
        members: &[String],
    ) -> Result<ReplicationStatus, StorageError> {
        let args = StatusArgs {
            folder: folder.to_string(),
            members: members.to_vec(),
        };
        let args_json = serde_json::to_string(&args)
            .map_err(|e| StorageError::Other(format!("encode `{STATUS_OP}` args: {e}")))?;
        let invoke = self.invoke.clone();
        let out = tokio::task::spawn_blocking(move || invoke(STATUS_OP, args_json))
            .await
            .map_err(|e| {
                StorageError::Transport(format!("`{STATUS_OP}` proxy task failed: {e}"))
            })??;
        serde_json::from_str(&out)
            .map_err(|e| StorageError::Other(format!("decode `{STATUS_OP}` result: {e}")))
    }
}

// Exercises the host-side `register_from_def` proxy + plugin-side `dispatch_op`,
// so it is owned by the `in-process` profile (the one that links tokio), matching
// the storage crate's own proxy tests.
#[cfg(all(test, feature = "in-process"))]
mod proxy_tests {
    use super::*;

    /// A trivial in-process provider used as the *plugin side* of the boundary:
    /// `dispatch_op` routes to it, and the host proxy reaches it through a thunk.
    struct FakeSync {
        name: String,
        healthy: bool,
    }

    #[orca_async]
    impl ReplicationStatusProvider for FakeSync {
        fn name(&self) -> &str {
            &self.name
        }
        async fn status(
            &self,
            folder: &str,
            members: &[String],
        ) -> Result<ReplicationStatus, StorageError> {
            Ok(ReplicationStatus {
                provider: self.name.clone(),
                healthy: self.healthy && !members.is_empty(),
                last_sync_ms: Some(42),
                detail: Some(format!("{folder}: {} members", members.len())),
            })
        }
    }

    /// The full round-trip: a registered proxy whose thunk stands in for the
    /// subprocess wire (decode args → produce the JSON a plugin's `dispatch_op`
    /// would), resolved through the public `resolve` seam the gate uses.
    #[tokio::test]
    async fn register_from_def_proxy_round_trips() {
        let thunk: crate::InvokeThunk = Arc::new(move |op: &str, args_json: String| {
            assert_eq!(op, STATUS_OP);
            let args: StatusArgs =
                serde_json::from_str(&args_json).map_err(|e| StorageError::Other(e.to_string()))?;
            let status = ReplicationStatus {
                provider: "syncthing".to_string(),
                healthy: !args.members.is_empty(),
                last_sync_ms: Some(42),
                detail: Some(format!("{}: {} members", args.folder, args.members.len())),
            };
            serde_json::to_string(&status).map_err(|e| StorageError::Other(e.to_string()))
        });

        register_from_def("syncthing".to_string(), thunk).expect("register");

        let members = vec!["10.0.0.10".to_string(), "10.0.0.11".to_string()];
        let status = resolve("syncthing", "media", &members)
            .await
            .expect("provider registered → Some");
        assert!(status.healthy, "both members present → healthy");
        assert_eq!(status.last_sync_ms, Some(42));
        assert_eq!(status.detail.as_deref(), Some("media: 2 members"));

        assert!(deregister_status_provider("syncthing"));
        // Gone → Unknown (the gate holds), the state core is in with no plugin.
        assert!(resolve("syncthing", "media", &members).await.is_none());
    }

    /// An unknown op is a wire error surfaced as `Err`, never a silent healthy.
    #[tokio::test]
    async fn dispatch_op_rejects_unknown_op() {
        let p = FakeSync {
            name: "syncthing".to_string(),
            healthy: true,
        };
        let e = dispatch_op(&p, "nope", serde_json::json!({}))
            .await
            .expect_err("unknown op errors");
        assert!(e.to_string().contains("no operation 'nope'"));
    }
}
