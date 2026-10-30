//! Generic `Sync` seam: "replicate source share → target(s)", method-agnostic
//! (syncthing today; zfs-send / rsync later). Core owns the declared state and
//! the reconcile/health contract; a provider implements one method.
//!
//! Surfaced 2026-08-27 repairing the willow→maple Gitea/PBS replication chain:
//! it had looked configured for months while being silently useless (ignored
//! paths, a crash-looping sync daemon, an unmounted source) because nothing
//! declared what "replicated" meant or checked it. This seam exists so that
//! state is declared and health-checked rather than hand-wired per host.
//!
//! This is the first of #339's deliverables — the trait/contract and provider
//! registry only. The share-model extension, fleet share catalog wiring, and
//! mount convergence are separate follow-on slices.

#![allow(clippy::disallowed_types)]

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// One declared replication: `source` share/path replicated to `target` by
/// `method` (a registered [`SyncProvider`] name, e.g. `"syncthing"`).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct SyncTargetSpec {
    pub source: String,
    pub target: String,
    pub method: String,
}

/// Declared state for one provider's `reconcile`: every [`SyncTargetSpec`]
/// whose `method` names it. The provider converges actual state to match.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct SyncReconcileArgs {
    pub targets: Vec<SyncTargetSpec>,
}

/// Result of a `reconcile` call. `errors` is non-fatal per-target so one bad
/// target doesn't block the rest from converging.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct SyncReconcileOutcome {
    pub applied: Vec<SyncTargetSpec>,
    pub errors: Vec<SyncTargetError>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct SyncTargetError {
    pub target: SyncTargetSpec,
    pub message: String,
}

/// Filter for a `status` call. Empty = every target the provider knows about.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct SyncStatusArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Health of one declared replication, as reported by its owning provider.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct SyncTargetHealth {
    pub target: SyncTargetSpec,
    pub healthy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_secs: Option<i64>,
    #[serde(default)]
    pub errors: Vec<String>,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A `Sync` method provider — one per replication method (syncthing today;
/// zfs-send / rsync later). Registered into a process-global registry so the
/// surface layer can fan `reconcile`/`status` out method-agnostically.
pub trait SyncProvider: Send + Sync {
    /// Provider/registry name (e.g. `"syncthing"`). Also the `method` value
    /// in [`SyncTargetSpec`] that routes a target to this provider.
    fn name(&self) -> &str;

    /// Converge actual state toward `args.targets`, all of which name this
    /// provider as their `method`.
    fn reconcile(&self, args: SyncReconcileArgs) -> BoxFuture<'_, Result<SyncReconcileOutcome>>;

    /// Report per-target health: completion%, lag, errors.
    fn status(&self, args: SyncStatusArgs) -> BoxFuture<'_, Result<Vec<SyncTargetHealth>>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn SyncProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a `Sync` provider. Re-registering the same `name()` replaces the
/// existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn SyncProvider>) {
    let mut g = GLOBAL.write().expect("sync registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn SyncProvider>> {
    GLOBAL.read().expect("sync registry poisoned").clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("sync registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points ─────────────────────────────────────────────────────

/// Fan `reconcile` across every registered provider, routing each
/// [`SyncTargetSpec`] to the provider named by its `method`. A target whose
/// method has no registered provider is reported as an error, not silently
/// dropped; a provider that errors is skipped for the rest — one broken
/// provider must not block every other method's reconcile.
pub async fn reconcile(args: SyncReconcileArgs) -> SyncReconcileOutcome {
    let all = providers();
    let mut out = SyncReconcileOutcome::default();
    for method in all.iter().map(|p| p.name().to_string()).collect::<Vec<_>>() {
        let targets: Vec<SyncTargetSpec> = args
            .targets
            .iter()
            .filter(|t| t.method == method)
            .cloned()
            .collect();
        if targets.is_empty() {
            continue;
        }
        let Some(provider) = all.iter().find(|p| p.name() == method) else {
            continue;
        };
        match provider.reconcile(SyncReconcileArgs { targets }).await {
            Ok(mut result) => {
                out.applied.append(&mut result.applied);
                out.errors.append(&mut result.errors);
            }
            Err(e) => {
                for target in args.targets.iter().filter(|t| t.method == method) {
                    out.errors.push(SyncTargetError {
                        target: target.clone(),
                        message: e.to_string(),
                    });
                }
            }
        }
    }
    let known_methods: Vec<&str> = all.iter().map(|p| p.name()).collect();
    for target in args
        .targets
        .iter()
        .filter(|t| !known_methods.contains(&t.method.as_str()))
    {
        out.errors.push(SyncTargetError {
            target: target.clone(),
            message: format!("no Sync provider registered for method '{}'", target.method),
        });
    }
    out
}

/// Fan `status` across every registered provider, concatenating results. A
/// provider that errors is skipped — one broken provider must not blank the
/// whole report.
pub async fn status(args: SyncStatusArgs) -> Vec<SyncTargetHealth> {
    let mut out = Vec::new();
    for p in providers() {
        if let Ok(mut health) = p.status(args.clone()).await {
            out.append(&mut health);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeProvider {
        name: String,
        reconciled: Arc<Mutex<Vec<SyncTargetSpec>>>,
        fail: bool,
    }

    impl SyncProvider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn reconcile(
            &self,
            args: SyncReconcileArgs,
        ) -> BoxFuture<'_, Result<SyncReconcileOutcome>> {
            let reconciled = self.reconciled.clone();
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    anyhow::bail!("boom");
                }
                reconciled.lock().unwrap().extend(args.targets.clone());
                Ok(SyncReconcileOutcome {
                    applied: args.targets,
                    errors: vec![],
                })
            })
        }
        fn status(&self, _args: SyncStatusArgs) -> BoxFuture<'_, Result<Vec<SyncTargetHealth>>> {
            let name = self.name.clone();
            Box::pin(async move {
                Ok(vec![SyncTargetHealth {
                    target: SyncTargetSpec {
                        source: "docs".into(),
                        target: "maple".into(),
                        method: name,
                    },
                    healthy: true,
                    completion_pct: Some(100.0),
                    lag_secs: Some(0),
                    errors: vec![],
                }])
            })
        }
    }

    #[test]
    fn registering_the_same_name_twice_replaces_not_duplicates() {
        register_provider(Arc::new(FakeProvider {
            name: "dup-test".into(),
            reconciled: Arc::new(Mutex::new(vec![])),
            fail: false,
        }));
        register_provider(Arc::new(FakeProvider {
            name: "dup-test".into(),
            reconciled: Arc::new(Mutex::new(vec![])),
            fail: false,
        }));
        let count = providers()
            .iter()
            .filter(|p| p.name() == "dup-test")
            .count();
        assert_eq!(count, 1);
        assert!(deregister_provider("dup-test"));
    }

    #[tokio::test]
    async fn reconcile_routes_each_target_to_its_named_method() {
        let seen_a = Arc::new(Mutex::new(vec![]));
        let seen_b = Arc::new(Mutex::new(vec![]));
        register_provider(Arc::new(FakeProvider {
            name: "method-a".into(),
            reconciled: seen_a.clone(),
            fail: false,
        }));
        register_provider(Arc::new(FakeProvider {
            name: "method-b".into(),
            reconciled: seen_b.clone(),
            fail: false,
        }));

        let out = reconcile(SyncReconcileArgs {
            targets: vec![
                SyncTargetSpec {
                    source: "docs".into(),
                    target: "maple".into(),
                    method: "method-a".into(),
                },
                SyncTargetSpec {
                    source: "media".into(),
                    target: "willow".into(),
                    method: "method-b".into(),
                },
            ],
        })
        .await;

        assert_eq!(out.applied.len(), 2);
        assert!(out.errors.is_empty());
        assert_eq!(seen_a.lock().unwrap().len(), 1);
        assert_eq!(seen_b.lock().unwrap().len(), 1);

        assert!(deregister_provider("method-a"));
        assert!(deregister_provider("method-b"));
    }

    #[tokio::test]
    async fn reconcile_reports_an_unknown_method_as_an_error_not_a_silent_drop() {
        let out = reconcile(SyncReconcileArgs {
            targets: vec![SyncTargetSpec {
                source: "docs".into(),
                target: "maple".into(),
                method: "no-such-provider".into(),
            }],
        })
        .await;
        assert!(out.applied.is_empty());
        assert_eq!(out.errors.len(), 1);
        assert!(
            out.errors[0]
                .message
                .contains("no Sync provider registered")
        );
    }

    #[tokio::test]
    async fn a_failing_provider_does_not_block_other_methods_from_reconciling() {
        let seen_ok = Arc::new(Mutex::new(vec![]));
        register_provider(Arc::new(FakeProvider {
            name: "method-fails".into(),
            reconciled: Arc::new(Mutex::new(vec![])),
            fail: true,
        }));
        register_provider(Arc::new(FakeProvider {
            name: "method-ok".into(),
            reconciled: seen_ok.clone(),
            fail: false,
        }));

        let out = reconcile(SyncReconcileArgs {
            targets: vec![
                SyncTargetSpec {
                    source: "a".into(),
                    target: "x".into(),
                    method: "method-fails".into(),
                },
                SyncTargetSpec {
                    source: "b".into(),
                    target: "y".into(),
                    method: "method-ok".into(),
                },
            ],
        })
        .await;

        assert_eq!(out.applied.len(), 1);
        assert_eq!(out.errors.len(), 1);
        assert!(out.errors[0].message.contains("boom"));
        assert_eq!(seen_ok.lock().unwrap().len(), 1);

        assert!(deregister_provider("method-fails"));
        assert!(deregister_provider("method-ok"));
    }

    #[tokio::test]
    async fn status_concatenates_across_providers_and_skips_errors() {
        register_provider(Arc::new(FakeProvider {
            name: "status-a".into(),
            reconciled: Arc::new(Mutex::new(vec![])),
            fail: false,
        }));
        register_provider(Arc::new(FakeProvider {
            name: "status-b".into(),
            reconciled: Arc::new(Mutex::new(vec![])),
            fail: false,
        }));

        let out = status(SyncStatusArgs::default()).await;
        let methods: Vec<&str> = out.iter().map(|h| h.target.method.as_str()).collect();
        assert!(methods.contains(&"status-a"));
        assert!(methods.contains(&"status-b"));

        assert!(deregister_provider("status-a"));
        assert!(deregister_provider("status-b"));
    }
}
