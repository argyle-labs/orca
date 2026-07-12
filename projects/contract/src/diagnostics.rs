//! Cross-crate diagnostics/repair types and provider registry.
//!
//! A diagnostics provider (raccoon for gaming, later bazzite/cachyos/beaver for
//! OS state) inspects a subsystem and emits typed [`Finding`]s, each optionally
//! carrying a [`RepairSpec`] describing how to remediate. The surface layer
//! (`dispatch::diagnostics_surface`) fans `diagnose` across every provider and
//! routes `repair` to the owning one, so the surface stays plugin-agnostic — the
//! same shape the `topology` and `storage` domains use.
//!
//! ## Provider registry
//!
//! A provider registers into a process-global registry — either in-process or,
//! for an external cdylib plugin, via the [`register_from_def`] JSON proxy the
//! plugin-loader installs for `domain = "diagnostics"`.

use std::sync::{Arc, LazyLock, RwLock};

use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BoxFuture;

/// How serious a [`Finding`] is. Ordered least→most severe.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Healthy — the checked state matches known-good.
    Ok,
    /// Informational nudge; nothing wrong, an optional improvement.
    Info,
    /// Degraded — works but should be fixed (the common actionable case).
    Warn,
    /// Broken — needs attention.
    Crit,
}

/// A repair fulfilled by invoking a managed-unit action on another capability —
/// possibly a *different* provider than the one that diagnosed the finding —
/// rather than the diagnosing provider's own `repair(id)`.
///
/// This is how a service provider proposes a fix that only its *runtime* can
/// apply: e.g. the Plex plugin detects its transcode scratch is undersized and
/// suggests growing the RAM of the LXC/VM that runs it, targeting that unit's
/// [`crate::unit::ACTION_SET_RESOURCES`] action. The service provider only
/// *proposes*; it never dispatches the runtime change itself. The surface layer
/// gates on [`RepairSpec::automatic`] / [`RepairSpec::privileged`] and, on
/// approval, dispatches this managed-unit action instead of calling `repair`.
/// Keeps "orca defines what, plugins define how": the proposer names a typed
/// capability call; core routes it to whichever provider owns the unit.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct DelegatedRepair {
    /// The managed unit the action targets (e.g. the LXC/VM running the service).
    pub unit: crate::unit::UnitId,
    /// The [`crate::unit::Verb::Update`] action to invoke on it (e.g.
    /// [`crate::unit::ACTION_SET_RESOURCES`]).
    pub action: String,
    /// Schema-validated JSON payload for the action, matching the target
    /// provider's declared schema. Carried as a JSON string across the FFI
    /// boundary (the same convention as [`crate::unit::UpdateArgs::payload`]).
    /// `None` = no payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

/// How to remediate a [`Finding`]. The `id` is passed back to `repair` to run it.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairSpec {
    /// Stable id for this repair, unique within its provider. Passed to
    /// `repair` to execute it (e.g. `"alsa-headroom"`, `"cpu-mode"`).
    pub id: String,
    /// Human-readable description of what running the repair does.
    pub description: String,
    /// Safe to run automatically (no privilege, no destructive side effects).
    /// `false` = surface it but require an explicit user action (confirmation).
    pub automatic: bool,
    /// Needs elevated privilege (admin / sudo / root) to apply.
    pub privileged: bool,
    /// When set, this repair is fulfilled by dispatching a managed-unit action on
    /// another capability (see [`DelegatedRepair`]) rather than the diagnosing
    /// provider's own `repair(id)`. `None` = the provider repairs it in-place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<DelegatedRepair>,
}

/// One diagnosed condition. Providers return a `Vec<Finding>` from `diagnose`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct Finding {
    /// Stable check id, unique within its provider (e.g. `"scx"`, `"cpu-mode"`).
    pub id: String,
    /// Provider that emitted this finding (registry key, e.g. `"raccoon"`).
    pub provider: String,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    /// How to fix it, when fixable. `None` for `Ok`/`Info` or unfixable states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairSpec>,
}

/// Result of running a repair.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairOutcome {
    /// The `RepairSpec.id` that was run.
    pub id: String,
    /// Provider that ran it.
    pub provider: String,
    /// Whether the repair applied successfully.
    pub ok: bool,
    /// Human-readable result (what was done, or why it couldn't be).
    pub message: String,
}

/// Filter for a `diagnose` fan-out. Empty = diagnose everything.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default)]
pub struct DiagnoseArgs {
    /// Restrict to a single provider by registry name. `None` = all providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Arguments to run one repair, routed to the provider that owns `repair_id`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct RepairArgs {
    /// Provider registry name that owns the repair.
    pub provider: String,
    /// The `RepairSpec.id` to run.
    pub repair_id: String,
    /// Explicit confirmation to execute a repair that is not `automatic` (the
    /// suggest-then-confirm gate). Required only for a [`DelegatedRepair`] whose
    /// spec has `automatic == false`; ignored for automatic and in-place repairs.
    /// Without it, such a repair returns a plan-only outcome describing the
    /// pending action instead of executing.
    #[serde(default)]
    pub confirm: bool,
}

// ── Provider registry ───────────────────────────────────────────────────────

/// A diagnostics/repair source — one per provider (raccoon, bazzite, …).
/// Registered into the process-global registry so the surface layer can fan out
/// across providers plugin-agnostically.
pub trait DiagnosticsProvider: Send + Sync {
    /// Provider/registry name (e.g. `"raccoon"`). Registry key; used to
    /// replace-in-place on re-register and to deregister on plugin unload.
    fn name(&self) -> &str;

    /// Inspect this provider's subsystem and return typed findings.
    fn diagnose(&self, args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>>;

    /// Run one repair by its `RepairSpec.id`.
    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>>;
}

static GLOBAL: LazyLock<RwLock<Vec<Arc<dyn DiagnosticsProvider>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a diagnostics provider. Re-registering the same `name()` replaces
/// the existing entry so a dev rebuild / plugin reload doesn't duplicate it.
pub fn register_provider(provider: Arc<dyn DiagnosticsProvider>) {
    let mut g = GLOBAL.write().expect("diagnostics registry poisoned");
    let name = provider.name().to_string();
    if let Some(slot) = g.iter_mut().find(|p| p.name() == name) {
        *slot = provider;
    } else {
        g.push(provider);
    }
}

/// Snapshot of every registered provider.
pub fn providers() -> Vec<Arc<dyn DiagnosticsProvider>> {
    GLOBAL
        .read()
        .expect("diagnostics registry poisoned")
        .clone()
}

/// Deregister the provider named `name`, if present. Returns `true` if removed.
pub fn deregister_provider(name: &str) -> bool {
    let mut g = GLOBAL.write().expect("diagnostics registry poisoned");
    let before = g.len();
    g.retain(|p| p.name() != name);
    before != g.len()
}

// ── Surface entry points (used by dispatch::diagnostics_surface) ─────────────

/// Fan `diagnose` across all registered providers (or one, if `args.provider`
/// is set), concatenating their findings. A provider that errors is skipped —
/// one broken provider must not blank the whole report.
pub async fn diagnose(args: DiagnoseArgs) -> Vec<Finding> {
    let mut out = Vec::new();
    for p in providers() {
        if let Some(want) = &args.provider
            && p.name() != want
        {
            continue;
        }
        match p.diagnose(args.clone()).await {
            Ok(mut findings) => out.append(&mut findings),
            Err(e) => out.push(Finding {
                id: "provider-error".to_string(),
                provider: p.name().to_string(),
                severity: Severity::Crit,
                title: format!("Provider '{}' failed to diagnose", p.name()),
                detail: e.to_string(),
                repair: None,
            }),
        }
    }
    out
}

/// Find the [`RepairSpec`] for `(provider, repair_id)` by re-diagnosing that
/// provider and matching the finding whose `repair.id == repair_id`. Returns
/// `None` when the provider reports no such repairable finding (e.g. the
/// condition already cleared). This is how the surface sees a repair's
/// [`RepairSpec::delegate`] at execution time — the caller only holds the id.
pub async fn find_repair_spec(provider: &str, repair_id: &str) -> Option<RepairSpec> {
    diagnose(DiagnoseArgs {
        provider: Some(provider.to_string()),
    })
    .await
    .into_iter()
    .find_map(|f| f.repair.filter(|r| r.id == repair_id))
}

/// Route one repair to the provider that owns it.
///
/// Most repairs are executed in place by the diagnosing provider's own
/// [`DiagnosticsProvider::repair`]. When the resolved [`RepairSpec`] carries a
/// [`RepairSpec::delegate`], the repair is instead fulfilled by dispatching a
/// managed-unit action on another capability (see [`DelegatedRepair`]):
///
/// 1. **Confirm gate** — a non-`automatic` delegated repair requires
///    `args.confirm == true`; without it, a plan-only [`RepairOutcome`]
///    (`ok == false`) describing the pending action is returned, nothing runs.
/// 2. **Dispatch** — the delegate's `Verb::Update` action is routed to whichever
///    unit provider owns [`DelegatedRepair::unit`] via [`crate::unit::dispatch`].
/// 3. **Re-diagnose** — the provider is re-diagnosed; `ok` reflects whether the
///    finding cleared. (Admin authorization for the delegated, runtime-mutating
///    path is enforced at the tool boundary, where `diagnostics.repair` is an
///    admin + data-mutation op.)
pub async fn repair(args: RepairArgs) -> Result<RepairOutcome> {
    let provider = providers()
        .into_iter()
        .find(|p| p.name() == args.provider)
        .ok_or_else(|| anyhow::anyhow!("no diagnostics provider named '{}'", args.provider))?;

    // Resolve the spec to see whether this repair delegates to a unit action.
    if let Some(spec) = find_repair_spec(&args.provider, &args.repair_id).await
        && let Some(delegate) = spec.delegate
    {
        // Confirm gate: a non-automatic delegated repair must be explicitly
        // confirmed. Return the plan rather than executing.
        if !spec.automatic && !args.confirm {
            return Ok(RepairOutcome {
                id: args.repair_id,
                provider: args.provider,
                ok: false,
                message: format!(
                    "confirmation required: will dispatch '{}' on unit '{}' ({}). \
                     Re-call with confirm=true to apply.",
                    delegate.action, delegate.unit.name, delegate.unit.manager
                ),
            });
        }

        // Dispatch the managed-unit action to whichever provider owns the unit.
        let outcome =
            crate::unit::dispatch(crate::unit::VerbArgs::Update(crate::unit::UpdateArgs {
                id: delegate.unit.clone(),
                action: delegate.action.clone(),
                payload: delegate.payload.clone(),
            }))
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "delegated repair '{}' failed to dispatch '{}' on unit '{}': {e}",
                    args.repair_id,
                    delegate.action,
                    delegate.unit.name
                )
            })?;

        // Re-diagnose: if the finding is gone, the delegate resolved it.
        let cleared = find_repair_spec(&args.provider, &args.repair_id)
            .await
            .is_none();
        return Ok(RepairOutcome {
            id: args.repair_id,
            provider: args.provider,
            ok: cleared,
            message: format!(
                "dispatched '{}' on unit '{}'; unit reported: {}; finding {}",
                delegate.action,
                delegate.unit.name,
                unit_outcome_summary(&outcome),
                if cleared { "cleared" } else { "still present" }
            ),
        });
    }

    // No delegate — the provider repairs it in place.
    provider.repair(args).await
}

/// A short human summary of a unit [`crate::unit::VerbOutcome`] for a repair
/// message — delegated repairs use `Verb::Update` actions, which return an
/// [`crate::unit::ActionOutcome`].
fn unit_outcome_summary(outcome: &crate::unit::VerbOutcome) -> String {
    match outcome {
        crate::unit::VerbOutcome::Action(a) => {
            format!("changed={}, {}", a.changed, a.message)
        }
        crate::unit::VerbOutcome::Item(_) => "item returned".to_string(),
        crate::unit::VerbOutcome::Items(_) => "items returned".to_string(),
    }
}

// ── cdylib FFI proxy ─────────────────────────────────────────────────────────

/// The synchronous invoke thunk a cdylib plugin's provider is driven through:
/// `(op, args_json) -> Result<result_json, error_string>`. Plain `Fn` of
/// strings so `contract` stays free of any ABI/loader dependency (no cycle).
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub type InvokeThunk =
    Arc<dyn Fn(&str, String) -> std::result::Result<String, String> + Send + Sync + 'static>;

/// Operation names the [`DiagnosticsProxy`] invokes across the FFI boundary. The
/// plugin exposes tools `"{invoke_prefix}.{DIAGNOSE_OP|REPAIR_OP}"`.
pub const DIAGNOSE_OP: &str = "diagnose";
pub const REPAIR_OP: &str = "repair";

/// Build and register a [`DiagnosticsProvider`] from a plugin backend descriptor
/// plus an [`InvokeThunk`]. The plugin-loader calls this from its domain
/// dispatch table for `domain = "diagnostics"`.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
pub fn register_from_def(name: String, invoke: InvokeThunk) -> Result<()> {
    register_provider(Arc::new(DiagnosticsProxy { name, invoke }));
    Ok(())
}

/// A [`DiagnosticsProvider`] backed by a cdylib plugin reached over the
/// JSON-proxy FFI boundary. Each op offloads the synchronous [`InvokeThunk`]
/// onto `spawn_blocking` and (de)serializes JSON at the seam.
///
/// Host-side cdylib proxy — in-process only; a thin build links no tokio.
#[cfg(feature = "in-process")]
struct DiagnosticsProxy {
    name: String,
    invoke: InvokeThunk,
}

#[cfg(feature = "in-process")]
impl DiagnosticsProxy {
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
                .map_err(|e| anyhow::anyhow!("diagnostics '{name}' {op} task panicked: {e}"))?
                .map_err(|e| anyhow::anyhow!("diagnostics '{name}' {op} failed: {e}"))?;
            serde_json::from_str(&out).map_err(|e| {
                anyhow::anyhow!("diagnostics '{name}' {op} returned invalid JSON: {e}")
            })
        })
    }
}

#[cfg(feature = "in-process")]
impl DiagnosticsProvider for DiagnosticsProxy {
    fn name(&self) -> &str {
        &self.name
    }

    fn diagnose(&self, args: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(DIAGNOSE_OP, args_json)
    }

    fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
        let args_json = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
        self.call(REPAIR_OP, args_json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::{ACTION_SET_RESOURCES, SetResourcesPayload, UnitId};

    #[test]
    fn repair_without_delegate_omits_it_in_json() {
        // An ordinary in-place repair: delegate is absent and must not serialize,
        // so existing findings round-trip byte-for-byte.
        let spec = RepairSpec {
            id: "clear-stale-scratch".into(),
            description: "Remove orphaned transcode scratch".into(),
            automatic: true,
            privileged: false,
            delegate: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(
            !json.contains("delegate"),
            "absent delegate must be skipped"
        );
        let round: RepairSpec = serde_json::from_str(&json).unwrap();
        assert!(round.delegate.is_none());
    }

    #[test]
    fn delegated_repair_targets_a_units_set_resources() {
        // A service provider proposes a fix its runtime must apply: grow the RAM
        // of the unit running it. confirm (!automatic) + admin (privileged).
        let payload = SetResourcesPayload {
            memory_mib: Some(8192),
            ..Default::default()
        };
        let spec = RepairSpec {
            id: "grow-runtime-ram".into(),
            description: "Transcode scratch is undersized; grow runtime RAM".into(),
            automatic: false,
            privileged: true,
            delegate: Some(DelegatedRepair {
                unit: UnitId {
                    manager: "proxmox@cluster-a".into(),
                    kind: "lxc".into(),
                    id: "110".into(),
                    name: "mediabox".into(),
                },
                action: ACTION_SET_RESOURCES.into(),
                payload: Some(serde_json::to_string(&payload).unwrap()),
            }),
        };
        // A confirm+admin suggested action carrying a cross-provider unit call.
        assert!(!spec.automatic);
        assert!(spec.privileged);
        let round: RepairSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        let d = round.delegate.expect("delegate present");
        assert_eq!(d.action, ACTION_SET_RESOURCES);
        assert_eq!(d.unit.id, "110");
        let got: SetResourcesPayload = serde_json::from_str(&d.payload.unwrap()).unwrap();
        assert_eq!(got.memory_mib, Some(8192));
    }

    // End-to-end delegate dispatch needs the tokio reactor + the unit registry.
    #[cfg(feature = "in-process")]
    mod flow {
        use super::*;
        use crate::BoxFuture;
        use crate::unit::{
            ActionDecl, ActionOutcome, KindDeclaration, UnitDescriptor, UnitProvider, VerbArgs,
            VerbDecl, VerbOutcome,
        };
        use std::sync::{Arc, Mutex};

        /// Diagnostics provider that emits one delegated finding until its shared
        /// `resolved` flag is set — modeling a condition the unit action clears.
        struct DelegatingProvider {
            name: String,
            unit_manager: String,
            resolved: Arc<Mutex<bool>>,
        }

        impl DiagnosticsProvider for DelegatingProvider {
            fn name(&self) -> &str {
                &self.name
            }
            fn diagnose(&self, _a: DiagnoseArgs) -> BoxFuture<'_, Result<Vec<Finding>>> {
                let (name, mgr, resolved) = (
                    self.name.clone(),
                    self.unit_manager.clone(),
                    self.resolved.clone(),
                );
                Box::pin(async move {
                    if *resolved.lock().unwrap() {
                        return Ok(vec![]);
                    }
                    Ok(vec![Finding {
                        id: "undersized".into(),
                        provider: name,
                        severity: Severity::Warn,
                        title: "scratch undersized".into(),
                        detail: "grow runtime RAM".into(),
                        repair: Some(RepairSpec {
                            id: "grow-ram".into(),
                            description: "grow runtime RAM".into(),
                            automatic: false,
                            privileged: true,
                            delegate: Some(DelegatedRepair {
                                unit: UnitId {
                                    manager: mgr,
                                    kind: "lxc".into(),
                                    id: "110".into(),
                                    name: "mediabox".into(),
                                },
                                action: ACTION_SET_RESOURCES.into(),
                                payload: Some(r#"{"memory_mib":8192}"#.into()),
                            }),
                        }),
                    }])
                })
            }
            fn repair(&self, args: RepairArgs) -> BoxFuture<'_, Result<RepairOutcome>> {
                // The in-place path must never run for a delegated repair.
                let name = self.name.clone();
                Box::pin(async move {
                    Ok(RepairOutcome {
                        id: args.repair_id,
                        provider: name,
                        ok: false,
                        message: "UNEXPECTED in-place repair".into(),
                    })
                })
            }
        }

        /// Unit provider that records the dispatched update and flips `resolved`.
        struct RecordingUnit {
            name: String,
            seen: Arc<Mutex<Vec<String>>>,
            resolved: Arc<Mutex<bool>>,
        }

        impl UnitProvider for RecordingUnit {
            fn name(&self) -> &str {
                &self.name
            }
            fn declarations(&self) -> Vec<KindDeclaration> {
                vec![KindDeclaration::new(
                    "lxc",
                    vec![VerbDecl::update(vec![ActionDecl::set_resources()])],
                )]
            }
            fn units(&self) -> BoxFuture<'_, Result<Vec<UnitDescriptor>>> {
                Box::pin(async { Ok(vec![]) })
            }
            fn invoke(&self, args: VerbArgs) -> BoxFuture<'_, Result<VerbOutcome>> {
                let (seen, resolved) = (self.seen.clone(), self.resolved.clone());
                Box::pin(async move {
                    if let VerbArgs::Update(u) = &args {
                        seen.lock().unwrap().push(format!(
                            "{}:{}",
                            u.action,
                            u.payload.clone().unwrap_or_default()
                        ));
                        *resolved.lock().unwrap() = true;
                    }
                    Ok(VerbOutcome::Action(ActionOutcome {
                        changed: true,
                        message: "resized".into(),
                    }))
                })
            }
        }

        #[tokio::test]
        async fn confirm_gate_returns_plan_and_runs_nothing() {
            let resolved = Arc::new(Mutex::new(false));
            let seen = Arc::new(Mutex::new(Vec::new()));
            register_provider(Arc::new(DelegatingProvider {
                name: "diag-gate".into(),
                unit_manager: "unit-gate@test".into(),
                resolved: resolved.clone(),
            }));
            crate::unit::register_provider(Arc::new(RecordingUnit {
                name: "unit-gate".into(),
                seen: seen.clone(),
                resolved: resolved.clone(),
            }));

            // No confirm on a non-automatic delegated repair → plan only.
            let out = repair(RepairArgs {
                provider: "diag-gate".into(),
                repair_id: "grow-ram".into(),
                confirm: false,
            })
            .await
            .unwrap();
            assert!(!out.ok, "unconfirmed repair must not report success");
            assert!(out.message.contains("confirmation required"));
            assert!(
                seen.lock().unwrap().is_empty(),
                "no unit action may be dispatched without confirmation"
            );

            assert!(deregister_provider("diag-gate"));
            assert!(crate::unit::deregister_provider("unit-gate"));
        }

        #[tokio::test]
        async fn confirmed_delegate_dispatches_unit_action_and_clears() {
            let resolved = Arc::new(Mutex::new(false));
            let seen = Arc::new(Mutex::new(Vec::new()));
            register_provider(Arc::new(DelegatingProvider {
                name: "diag-run".into(),
                unit_manager: "unit-run@test".into(),
                resolved: resolved.clone(),
            }));
            crate::unit::register_provider(Arc::new(RecordingUnit {
                name: "unit-run".into(),
                seen: seen.clone(),
                resolved: resolved.clone(),
            }));

            let out = repair(RepairArgs {
                provider: "diag-run".into(),
                repair_id: "grow-ram".into(),
                confirm: true,
            })
            .await
            .unwrap();

            // The unit action was dispatched with the delegate's payload,
            // routed by manager_base to the recording unit provider.
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "exactly one unit action dispatched");
            assert!(seen[0].starts_with(ACTION_SET_RESOURCES));
            assert!(seen[0].contains("memory_mib"));
            // Re-diagnose saw the finding cleared → ok.
            assert!(
                out.ok,
                "cleared finding must report success: {}",
                out.message
            );
            assert!(out.message.contains("cleared"));
            assert!(!out.message.contains("UNEXPECTED"));

            assert!(deregister_provider("diag-run"));
            assert!(crate::unit::deregister_provider("unit-run"));
        }
    }
}
