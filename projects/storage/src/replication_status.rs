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
