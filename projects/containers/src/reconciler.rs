//! Auto-start reconciler with stale-mount awareness.
//!
//! C3 is the auto-start half of the reconciler: walk every registered
//! [`crate::RuntimeAdapter`], decide a [`ReconcileAction`] per
//! container per the decision table below, and (in the non-dry
//! variant) execute it.
//!
//! ## Decision table
//!
//! | restart_policy        | container state                | action                                |
//! |-----------------------|--------------------------------|---------------------------------------|
//! | unless-stopped/always | created / exited(code=0) / dead | start; emit `containers.started`     |
//! | unless-stopped/always | exited(code!=0)                | start tentative; arm breaker hook    |
//! | no / on-failure       | any non-running                | leave; emit `containers.skipped_policy` |
//! | (any)                 | running / starting / paused / stopping / unknown | no-op (not a candidate) |
//!
//! Label overrides — checked **before** the policy lookup:
//!
//! - `orca.skip=true` → emit `containers.skipped_label` (reason
//!   [`SkipLabelReason::Skip`]); never act.
//! - `orca.heal=manual` → emit `containers.skipped_label` (reason
//!   [`SkipLabelReason::Manual`]); never act.
//!
//! ## Stale-mount gate
//!
//! Before any start, every bind mount source on the candidate is
//! probed via [`MountProbe::probe`]. On `ESTALE` (Linux raw OS error
//! `116`), the reconciler emits
//! `containers.start_blocked_stale_mount` (warn) with the list of
//! offending sources and records [`ReconcileAction::BlockedStaleMount`].
//! Non-bind mounts (`StorageRef` variants from the Proxmox adapter,
//! etc.) are not probed.
//!
//! Inline stale probe is a C3 expedient; later C-series work replaces
//! this with an event subscription from the mounts reconciler
//! ([[stale-mount-detection]] / §2.2).
//!
//! ## Breaker seam
//!
//! For tentative starts (exited non-zero) the reconciler consults
//! [`crate::breaker::arm`]. C3's stub always returns
//! [`crate::breaker::BreakerDecision::Proceed`]; the
//! `containers.held_pending_breaker` event is wired but stub-fired
//! only. C4 swaps in the real policy without touching call sites.
//!
//! ## Notifier wiring
//!
//! Events flow through [`notifications::Dispatcher`] with typed event
//! payload structs ([`StartedPayload`], [`SkippedPolicyPayload`],
//! [`SkippedLabelPayload`], [`StaleMountBlockedPayload`],
//! [`HeldPendingBreakerPayload`]). The dispatcher decides which
//! backend (ntfy / email / Slack / etc.) renders each event — this
//! crate stays backend-agnostic
//! ([[feedback-notifications-backend-agnostic]]).

use crate::breaker::{self, ArmRequest, BreakerDecision, BreakerStore, HoldReason, MemoryStore};
use crate::{
    Container, ContainerState, ListFilter, RestartPolicy, RuntimeAdapter, RuntimeKind,
    registered_adapters,
};
use notifications::{Dispatcher, EmitOutcome, Event, EventClass, Severity};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

// ── Decision-table types ──────────────────────────────────────────────────

/// Closed enum of actions the reconciler can take per container. The
/// reconciler stamps exactly one of these per candidate row; opaque
/// blob variants are deliberately absent ([[feedback-no-serde-json-value]]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileAction {
    /// Container was successfully started (or, in dry mode, would have
    /// been started).
    Started,
    /// Restart policy is `no`/`on-failure`; reconciler left it alone.
    SkippedPolicy,
    /// Container carries `orca.skip=true` or `orca.heal=manual`;
    /// reconciler left it alone.
    SkippedLabel,
    /// At least one bind mount source returned ESTALE; the start was
    /// gated and the container left in its observed state.
    BlockedStaleMount,
    /// Container was a tentative start (exited non-zero) and the
    /// breaker returned [`BreakerDecision::Hold`]. C3 wires this but
    /// the stub breaker never returns it; C4 flips it on.
    HeldPendingBreaker,
    /// Candidate not eligible — already running, paused, mid-
    /// transition, or in an unknown state.
    NoOp,
}

/// Typed reason field on a [`ReconcileRow`]. Each variant carries
/// exactly the context the operator needs to understand the action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReconcileReason {
    /// Started, no breaker involvement.
    StartedClean,
    /// Started after a non-zero exit; breaker armed and returned
    /// `Proceed`.
    StartedTentative { exit_code: Option<i32> },
    /// Restart policy is `no`/`on-failure`.
    PolicyNotAutoStart { policy: RestartPolicy },
    /// `orca.skip=true` or `orca.heal=manual` label override.
    LabelOverride { reason: SkipLabelReason },
    /// Stale mount(s) blocked the start.
    StaleMount { blocked_sources: Vec<PathBuf> },
    /// Breaker returned `Hold` for a tentative start.
    BreakerHeld { exit_code: Option<i32> },
    /// Container is in a state the reconciler does not act on.
    NotACandidate { state: ContainerState },
}

/// Which label triggered a skip. Closed — there are exactly two label
/// gates in C3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkipLabelReason {
    /// `orca.skip=true` — operator escape hatch, never touch.
    Skip,
    /// `orca.heal=manual` — notify-only, never act.
    Manual,
}

/// One row in [`ReconcileOutput::rows`]: the action the reconciler
/// took (or would have taken in dry mode) for one container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileRow {
    pub host: String,
    pub runtime: RuntimeKind,
    pub id: String,
    pub name: String,
    pub action: ReconcileAction,
    pub reason: ReconcileReason,
}

/// Per-adapter `list()` failure surfaced alongside the rows from the
/// adapters that succeeded. Mirrors
/// [`crate::AdapterListError`] but carries [`RuntimeKind`] typed so
/// the reconciler can reason about it without re-parsing the string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdapterFailure {
    pub runtime: RuntimeKind,
    pub message: String,
}

/// Result of one reconcile pass — typed, exhaustively enumerated, no
/// opaque JSON value types anywhere on the surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileOutput {
    /// True when the call ran in dry mode (no `.start()` calls, no
    /// breaker arming). Stale-mount probes still ran — they're read-
    /// only.
    pub dry_run: bool,
    /// One row per container the reconciler considered.
    pub rows: Vec<ReconcileRow>,
    /// Adapter `list()` failures. An adapter erroring on `list` does
    /// not fail the whole reconcile.
    pub adapter_errors: Vec<AdapterFailure>,
    /// Adapter `start()` failures captured during execution. Empty in
    /// dry mode.
    pub start_errors: Vec<StartFailure>,
}

/// A `.start()` call that the adapter rejected. The row that triggered
/// the call is still in [`ReconcileOutput::rows`] with action
/// [`ReconcileAction::Started`] because the reconciler's *decision*
/// was to start it; the failure to actually start is recorded
/// separately so consumers can pair the two.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StartFailure {
    pub host: String,
    pub runtime: RuntimeKind,
    pub id: String,
    pub name: String,
    pub message: String,
}

// ── Typed event payloads ─────────────────────────────────────────────────

/// `containers.started` — info severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartedPayload {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    pub container_name: String,
    pub restart_policy: RestartPolicy,
    /// Populated when the container exited non-zero before the start.
    pub exit_code: Option<i32>,
    /// True when this start was classified tentative (exited non-zero)
    /// and went through the breaker.
    ///
    /// **Docker-only today.** The LXC adapter does not surface an
    /// `exit_code`, so the tentative gate (`exit_code != Some(0)`) is
    /// always false for LXC and this field never goes true on that path.
    /// Tracked as #11 in [[project-breaker-followup-6-bear-punch-list]];
    /// a future fix may compute it from the breaker arming gate so it
    /// has meaning for runtimes without exit codes.
    pub tentative: bool,
}

/// `containers.skipped_policy` — debug severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedPolicyPayload {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    pub container_name: String,
    pub restart_policy: RestartPolicy,
    pub state: ContainerState,
}

/// `containers.skipped_label` — debug severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedLabelPayload {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    pub container_name: String,
    pub reason: SkipLabelReason,
}

/// `containers.start_blocked_stale_mount` — warn severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleMountBlockedPayload {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    pub container_name: String,
    pub blocked_sources: Vec<PathBuf>,
}

/// `containers.held_pending_breaker` — warn severity. C4 turns this
/// on; the breaker's classifier returns the trip reason, which is
/// stamped into the payload so the operator sees *why* the start was
/// held without having to re-run classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldPendingBreakerPayload {
    pub host: String,
    pub runtime: RuntimeKind,
    pub container_id: String,
    pub container_name: String,
    pub exit_code: Option<i32>,
    /// Closed enum of trip reasons from the breaker classifier.
    pub hold_reason: HoldReason,
    /// True when the container is currently running and the hold applies
    /// to the *next* start — fired from the observe-only NoOp branch.
    /// False when the hold blocked an in-flight start — fired from the
    /// start-pipeline path. Drives the body text so operators don't read
    /// "HELD start of X" for a container that's running fine right now.
    #[serde(default)]
    pub currently_running: bool,
}

// ── Mount probe ───────────────────────────────────────────────────────────

/// `ESTALE` on Linux. Defined as a raw constant rather than pulled in
/// via `libc` because the workspace doesn't take a libc dep for a
/// single integer ([[feedback-no-thin-wrappers]] — don't grow the dep
/// graph for something you can spell directly).
#[cfg(target_os = "linux")]
pub const ESTALE_RAW: i32 = 116;
/// `ESTALE` on macOS / *BSD — same name, different value, picked off
/// the `sys/errno.h` header. Wired so the probe compiles on dev
/// laptops; the path that matters in production is the Linux one.
#[cfg(not(target_os = "linux"))]
pub const ESTALE_RAW: i32 = 70;

/// Pluggable bind-mount source probe. The default
/// [`RealMountProbe`] hits the filesystem with
/// `std::fs::metadata`; tests inject a [`FakeMountProbe`] that returns
/// canned answers without touching the host filesystem.
pub trait MountProbe: Send + Sync {
    /// Probe `source`. Returns
    /// [`MountProbeResult::Stale`] when the OS reports ESTALE,
    /// [`MountProbeResult::Ok`] when metadata succeeded, and
    /// [`MountProbeResult::OtherError`] for any other I/O error
    /// (permission, timeout, …). The reconciler treats `OtherError`
    /// as *not* a stale block — those errors are noted but the start
    /// proceeds, mirroring §2.1's "only ESTALE gates auto-start".
    fn probe(&self, source: &std::path::Path) -> MountProbeResult;
}

/// Probe outcome. Closed enum — no `Other(String)` blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountProbeResult {
    /// Source resolves; the bind is healthy.
    Ok,
    /// Source returned ESTALE — blocks the start.
    Stale,
    /// Source returned a non-ESTALE error. Recorded but does not
    /// block.
    OtherError { os_error: Option<i32> },
}

/// Default probe: `std::fs::metadata` with ESTALE classification.
pub struct RealMountProbe;

impl MountProbe for RealMountProbe {
    fn probe(&self, source: &std::path::Path) -> MountProbeResult {
        match std::fs::metadata(source) {
            Ok(_) => MountProbeResult::Ok,
            Err(e) => {
                let os = e.raw_os_error();
                if os == Some(ESTALE_RAW) {
                    MountProbeResult::Stale
                } else {
                    MountProbeResult::OtherError { os_error: os }
                }
            }
        }
    }
}

// ── Reconciler entry points ───────────────────────────────────────────────

/// Inputs for a reconcile pass. Pulled into a struct so the public
/// surface stays one function-call wide as the C-series grows.
pub struct ReconcileInput<'a> {
    pub adapters: Vec<Arc<dyn RuntimeAdapter>>,
    pub probe: &'a dyn MountProbe,
    pub dispatcher: Option<&'a Dispatcher>,
    /// Persistence boundary for the crashloop circuit breaker. The
    /// reconciler consults this on every tentative start (exited non-
    /// zero) to decide whether to proceed or short-circuit to
    /// [`ReconcileAction::HeldPendingBreaker`].
    ///
    /// The tool-surface entry points wire in a process-local
    /// [`MemoryStore`] until the plugin-namespaced db primitive
    /// (`project_sdk_plugin_namespaced_db`) lands and a `FileStore` or
    /// db-backed impl takes its place.
    pub breaker_store: &'a dyn BreakerStore,
    pub dry_run: bool,
}

/// One reconcile pass. Walks every adapter, classifies every container
/// per the decision table, executes the action (when not dry), and
/// emits typed events through the dispatcher when one is supplied.
pub async fn reconcile(input: ReconcileInput<'_>) -> ReconcileOutput {
    let filter = ListFilter {
        all: true,
        labels: Vec::new(),
    };
    let mut rows: Vec<ReconcileRow> = Vec::new();
    let mut adapter_errors: Vec<AdapterFailure> = Vec::new();
    let mut start_errors: Vec<StartFailure> = Vec::new();
    // Live `(host, runtime, container_id)` keys observed this pass. Drives
    // the once-per-reconcile breaker/wedge store GC so persisted records for
    // containers that no longer exist don't accumulate unbounded. An adapter
    // that errors on `list()` contributes no keys and flips
    // `all_adapters_listed` false — we then skip the store GC entirely so a
    // transient list failure can never evict a live record.
    let mut live_keys: std::collections::HashSet<(String, RuntimeKind, String)> =
        std::collections::HashSet::new();
    let mut all_adapters_listed = true;

    // Wedge detection + auto-recovery is gated on a real dispatcher.
    // Without one there is no operator-visible escalation path, so
    // probing + persisting state would be silent (and tests that pass
    // `dispatcher: None` would write into `~/.orca/containers/`).
    let wedge_store: Option<Box<dyn crate::wedge::WedgeStore>> =
        if !input.dry_run && input.dispatcher.is_some() {
            Some(default_wedge_store())
        } else {
            None
        };

    for adapter in &input.adapters {
        let kind = adapter.kind();
        let containers = match adapter.list(&filter).await {
            Ok(cs) => cs,
            Err(e) => {
                adapter_errors.push(AdapterFailure {
                    runtime: kind,
                    message: e.to_string(),
                });
                all_adapters_listed = false;
                continue;
            }
        };
        for container in containers {
            live_keys.insert((
                container.host.clone(),
                container.runtime,
                container.id.clone(),
            ));
            let row = classify(&container);
            // Some classifications need to actually do work (start +
            // probe + breaker). Others just emit an event and move on.
            match row.action {
                ReconcileAction::SkippedLabel => {
                    emit_skipped_label(input.dispatcher, &container, &row).await;
                    rows.push(row);
                }
                ReconcileAction::SkippedPolicy => {
                    emit_skipped_policy(input.dispatcher, &container).await;
                    rows.push(row);
                }
                ReconcileAction::NoOp => {
                    // LXC observe-only arm. A NoOp row means classify
                    // decided no start is needed this tick (container
                    // is running, or its policy isn't auto-start). For
                    // LXC with an auto-restart policy we still arm the
                    // breaker every tick — the journalctl tail +
                    // cross-tick `last_observed_state` only update if
                    // `arm()` runs, so without this the transition
                    // counter and journal-failure classifier stay
                    // dormant for healthy-looking-but-flapping
                    // containers. `initiating_start: false` keeps the
                    // start-intent counters untouched: this is pure
                    // observation. If the breaker trips, the helper
                    // notifies (with per-hold suppression) and
                    // persists `Held`; the row stays `NoOp` because
                    // the container *is* currently running — the hold
                    // takes effect the next time we'd start it.
                    if !input.dry_run
                        && breaker::arm_on_every_start(container.runtime)
                        && container.restart_policy.desires_running()
                    {
                        // Discard the returned HoldReason: the row stays
                        // NoOp by design on the observe-only path — the
                        // hold only blocks the *next* start, and the
                        // helper has already notified internally.
                        let _ = arm_and_dispatch_hold(
                            adapter.as_ref(),
                            &container,
                            input.breaker_store,
                            input.dispatcher,
                            /* initiating_start */ false,
                        )
                        .await;
                    }
                    // Wedge probe runs on currently-running, auto-
                    // restart containers. State machine + dispatch +
                    // recovery is delegated to `handle_wedge_observation`.
                    // TODO: gate on external healthcheck failure once
                    // that domain lands — until then we probe every
                    // tick, which is cheap (Live = single API/exec call).
                    if let Some(store) = wedge_store.as_deref()
                        && container.state == ContainerState::Running
                        && container.restart_policy.desires_running()
                    {
                        handle_wedge_observation(
                            adapter.as_ref(),
                            &container,
                            store,
                            input.dispatcher,
                        )
                        .await;
                    }
                    rows.push(row);
                }
                ReconcileAction::Started
                | ReconcileAction::BlockedStaleMount
                | ReconcileAction::HeldPendingBreaker => {
                    // The classifier optimistically stamps
                    // `Started` for candidates; we still have to run
                    // the stale-mount gate + breaker here, which may
                    // downgrade the row.
                    let resolved = run_start_pipeline(
                        adapter.as_ref(),
                        &container,
                        input.probe,
                        input.breaker_store,
                        input.dispatcher,
                        input.dry_run,
                        &mut start_errors,
                    )
                    .await;
                    rows.push(resolved);
                }
            }
        }
    }

    // Once-per-pass store GC. Skipped on dry runs (read-only contract) and
    // when any adapter failed to list (a partial view of the fleet must not
    // evict records for containers we simply couldn't see this tick).
    // Storage errors here are non-fatal: GC is a safety-net, not a critical
    // path — log and carry on rather than failing the whole reconcile.
    if !input.dry_run && all_adapters_listed {
        if let Err(e) = input.breaker_store.retain_active(&live_keys) {
            tracing::warn!(
                target: "containers::breaker",
                "breaker store retain_active failed: {e}",
            );
        }
        if let Some(store) = wedge_store.as_deref()
            && let Err(e) = store.retain_active(&live_keys)
        {
            tracing::warn!(
                target: "containers::wedge",
                "wedge store retain_active failed: {e}",
            );
        }
    }

    rows.sort_by(|a, b| {
        a.host
            .cmp(&b.host)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });

    ReconcileOutput {
        dry_run: input.dry_run,
        rows,
        adapter_errors,
        start_errors,
    }
}

// ── Wedge integration ─────────────────────────────────────────────────────

/// One wedge tick for one Running container. Loads the prior record,
/// runs `process_liveness_observation`, dispatches the emitted
/// `WedgeEvent`s, applies the `NextAction`. On `AttemptRecovery` calls
/// `wedge::attempt_unwedge` and chains `process_recovery_outcome`,
/// dispatching its events and applying its action too.
///
/// Storage errors log + return (safety-net, not a critical path —
/// same posture the breaker takes). Dispatcher errors are absorbed
/// inside `emit_wedge_event`; one failed backend doesn't stop the
/// rest of the loop.
async fn handle_wedge_observation(
    adapter: &dyn RuntimeAdapter,
    container: &Container,
    store: &dyn crate::wedge::WedgeStore,
    dispatcher: Option<&Dispatcher>,
) {
    use crate::wedge::{self, NextAction};

    let observed = adapter.probe_liveness(container).await;
    let prior = match store.load(&container.host, container.runtime, &container.id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "containers::wedge",
                host = %container.host,
                runtime = container.runtime.as_str(),
                id = %container.id,
                "wedge store load failed: {e}",
            );
            return;
        }
    };
    let has_recoverer = adapter.wedge_recoverer().is_some();
    let now = utils::time::now();
    let result =
        wedge::process_liveness_observation(prior, observed, container, has_recoverer, now);

    for ev in &result.events {
        emit_wedge_event(dispatcher, ev).await;
    }

    match result.next_action {
        NextAction::Noop => {}
        NextAction::Delete => {
            if let Err(e) = store.delete(&container.host, container.runtime, &container.id) {
                tracing::warn!(
                    target: "containers::wedge",
                    host = %container.host,
                    runtime = container.runtime.as_str(),
                    id = %container.id,
                    "wedge store delete failed: {e}",
                );
            }
        }
        NextAction::Save(record) => {
            if let Err(e) = store.save(&record) {
                tracing::warn!(
                    target: "containers::wedge",
                    host = %container.host,
                    runtime = container.runtime.as_str(),
                    id = %container.id,
                    "wedge store save failed: {e}",
                );
            }
        }
        NextAction::AttemptRecovery(record) => {
            // Persist intent first so a crash mid-attempt doesn't lose
            // the recovery_attempts counter.
            if let Err(e) = store.save(&record) {
                tracing::warn!(
                    target: "containers::wedge",
                    host = %container.host,
                    runtime = container.runtime.as_str(),
                    id = %container.id,
                    "wedge store save (pre-attempt) failed: {e}",
                );
                return;
            }
            let outcome = match wedge::attempt_unwedge(adapter, container).await {
                Ok(o) => o,
                Err(e) => {
                    // Refused = no recoverer; defensively clear the
                    // record so we don't loop forever on a runtime
                    // that can't recover. `process_liveness_observation`
                    // with `has_recoverer=false` already escalated.
                    tracing::warn!(
                        target: "containers::wedge",
                        host = %container.host,
                        runtime = container.runtime.as_str(),
                        id = %container.id,
                        "attempt_unwedge refused: {e}",
                    );
                    return;
                }
            };
            let follow =
                wedge::process_recovery_outcome(record, &outcome, container, utils::time::now());
            for ev in &follow.events {
                emit_wedge_event(dispatcher, ev).await;
            }
            match follow.next_action {
                NextAction::Noop => {}
                NextAction::Delete => {
                    if let Err(e) = store.delete(&container.host, container.runtime, &container.id)
                    {
                        tracing::warn!(
                            target: "containers::wedge",
                            host = %container.host,
                            runtime = container.runtime.as_str(),
                            id = %container.id,
                            "wedge store delete (post-attempt) failed: {e}",
                        );
                    }
                }
                NextAction::Save(r) => {
                    if let Err(e) = store.save(&r) {
                        tracing::warn!(
                            target: "containers::wedge",
                            host = %container.host,
                            runtime = container.runtime.as_str(),
                            id = %container.id,
                            "wedge store save (post-attempt) failed: {e}",
                        );
                    }
                }
                // `process_recovery_outcome` only returns Save or
                // Delete — but pattern-match exhaustively rather than
                // unreachable!() so the build stays clean if the enum
                // gains variants.
                NextAction::AttemptRecovery(_) => {}
            }
        }
    }
}

/// Map a `WedgeEvent` to a typed `notifications::Event` and emit it.
/// Severity grades: `Detected`/`RecoveryAttempted` = Warn,
/// `RecoverySucceeded`/`Recovered` = Info, `RecoveryFailed` = Warn,
/// `Unrecoverable` = Error (page-an-operator).
async fn emit_wedge_event(dispatcher: Option<&Dispatcher>, ev: &crate::wedge::WedgeEvent) {
    use crate::wedge::WedgeEvent as W;
    let Some(d) = dispatcher else {
        return;
    };
    let (severity, title, host) = match ev {
        W::Detected {
            host,
            container_name,
            ..
        } => (
            Severity::Warn,
            format!("containers.wedge_detected: {container_name}"),
            host.clone(),
        ),
        W::RecoveryAttempted {
            host,
            container_name,
            attempt_number,
            ..
        } => (
            Severity::Warn,
            format!("containers.wedge_recovery_attempt {attempt_number}: {container_name}"),
            host.clone(),
        ),
        W::RecoverySucceeded {
            host,
            container_name,
            ..
        } => (
            Severity::Info,
            format!("containers.wedge_recovered: {container_name}"),
            host.clone(),
        ),
        W::RecoveryFailed {
            host,
            container_name,
            attempt_number,
            ..
        } => (
            Severity::Warn,
            format!(
                "containers.wedge_recovery_failed (attempt {attempt_number}): {container_name}"
            ),
            host.clone(),
        ),
        W::Unrecoverable {
            host,
            container_name,
            ..
        } => (
            Severity::Error,
            format!("containers.wedge_unrecoverable: {container_name}"),
            host.clone(),
        ),
        W::Recovered {
            host,
            container_name,
            ..
        } => (
            Severity::Info,
            format!("containers.wedge_recovered_spontaneously: {container_name}"),
            host.clone(),
        ),
    };
    let event = Event::new(EventClass::Alert, severity, title, "reconciler:containers")
        .with_host(host)
        .with_body(render_wedge_body(ev));
    let _ = d.emit(&event).await;
}

fn render_wedge_body(ev: &crate::wedge::WedgeEvent) -> String {
    use crate::wedge::WedgeEvent as W;
    match ev {
        W::Detected {
            host,
            runtime,
            container_id,
            container_name,
            first_wedged_at,
        } => format!(
            "WEDGED `{container_name}` ({runtime}:{container_id}) on `{host}` — first wedged at {first_wedged_at}",
            runtime = runtime.as_str(),
            first_wedged_at = first_wedged_at.to_rfc3339(),
        ),
        W::RecoveryAttempted {
            host,
            runtime,
            container_id,
            container_name,
            attempt_number,
        } => format!(
            "attempting recovery {attempt_number}/{max} of `{container_name}` ({runtime}:{container_id}) on `{host}`",
            runtime = runtime.as_str(),
            max = crate::wedge::MAX_RECOVERY_ATTEMPTS,
        ),
        W::RecoverySucceeded {
            host,
            runtime,
            container_id,
            container_name,
            attempts_taken,
            total_wedged_duration_secs,
        } => format!(
            "recovered `{container_name}` ({runtime}:{container_id}) on `{host}` after {attempts_taken} attempt(s), wedged for {total_wedged_duration_secs:.0}s",
            runtime = runtime.as_str(),
        ),
        W::RecoveryFailed {
            host,
            runtime,
            container_id,
            container_name,
            attempt_number,
            error,
        } => format!(
            "recovery attempt {attempt_number} FAILED for `{container_name}` ({runtime}:{container_id}) on `{host}` — {error}",
            runtime = runtime.as_str(),
        ),
        W::Unrecoverable {
            host,
            runtime,
            container_id,
            container_name,
            attempts,
            first_wedged_at,
        } => format!(
            "UNRECOVERABLE: `{container_name}` ({runtime}:{container_id}) on `{host}` — {attempts} recovery attempts failed, first wedged at {first_wedged_at}. Manual intervention required (try `orca containers.unwedge`).",
            runtime = runtime.as_str(),
            first_wedged_at = first_wedged_at.to_rfc3339(),
        ),
        W::Recovered {
            host,
            runtime,
            container_id,
            container_name,
            attempts,
            total_wedged_duration_secs,
        } => format!(
            "`{container_name}` ({runtime}:{container_id}) on `{host}` returned to live spontaneously after {attempts} attempt(s) ({total_wedged_duration_secs:.0}s wedged)",
            runtime = runtime.as_str(),
        ),
    }
}

/// Pick the right `WedgeStore` for the reconciler: `FileStore` rooted
/// at `<orca_home>/containers/wedge_state.json` when
/// [`crate::wedge::FileStore::default_path`] resolves, otherwise a
/// process-local `MemoryStore`. Mirrors [`default_breaker_store`].
fn default_wedge_store() -> Box<dyn crate::wedge::WedgeStore> {
    match crate::wedge::FileStore::default_path() {
        Some(dir) => Box::new(crate::wedge::FileStore::new(dir)),
        None => {
            tracing::warn!(
                target: "containers::wedge",
                "neither ORCA_HOME nor HOME set; using in-memory wedge store (state lost on restart)"
            );
            Box::new(crate::wedge::MemoryStore::new())
        }
    }
}

/// Pure classification of one container against the decision table.
/// Label overrides win, then policy, then state. The optimistic
/// `Started` stamp is downgraded by [`run_start_pipeline`] if the
/// stale-mount gate or breaker say so.
fn classify(c: &Container) -> ReconcileRow {
    let header = |action: ReconcileAction, reason: ReconcileReason| ReconcileRow {
        host: c.host.clone(),
        runtime: c.runtime,
        id: c.id.clone(),
        name: c.name.clone(),
        action,
        reason,
    };

    // Label overrides — checked first.
    if c.has_label("orca.skip", "true") {
        return header(
            ReconcileAction::SkippedLabel,
            ReconcileReason::LabelOverride {
                reason: SkipLabelReason::Skip,
            },
        );
    }
    if c.has_label("orca.heal", "manual") {
        return header(
            ReconcileAction::SkippedLabel,
            ReconcileReason::LabelOverride {
                reason: SkipLabelReason::Manual,
            },
        );
    }

    // Anything not desired-running per policy → either SkippedPolicy
    // (if it isn't already running) or NoOp (if it is — operator may
    // have started it manually).
    if !c.restart_policy.desires_running() {
        match c.state {
            ContainerState::Running
            | ContainerState::Starting
            | ContainerState::Paused
            | ContainerState::Stopping => {
                return header(
                    ReconcileAction::NoOp,
                    ReconcileReason::NotACandidate { state: c.state },
                );
            }
            ContainerState::Created
            | ContainerState::Exited
            | ContainerState::Dead
            | ContainerState::Unknown => {
                return header(
                    ReconcileAction::SkippedPolicy,
                    ReconcileReason::PolicyNotAutoStart {
                        policy: c.restart_policy,
                    },
                );
            }
        }
    }

    // Desired-running. Decide based on observed state.
    match c.state {
        ContainerState::Running
        | ContainerState::Starting
        | ContainerState::Paused
        | ContainerState::Stopping => header(
            ReconcileAction::NoOp,
            ReconcileReason::NotACandidate { state: c.state },
        ),
        ContainerState::Unknown => header(
            ReconcileAction::NoOp,
            ReconcileReason::NotACandidate { state: c.state },
        ),
        ContainerState::Created | ContainerState::Dead => {
            header(ReconcileAction::Started, ReconcileReason::StartedClean)
        }
        ContainerState::Exited => {
            // Clean exit (code 0 or missing) — auto-start clean.
            // Non-zero exit — tentative; breaker check happens in the
            // pipeline.
            let exit = c.exit_code;
            if matches!(exit, None | Some(0)) {
                header(ReconcileAction::Started, ReconcileReason::StartedClean)
            } else {
                header(
                    ReconcileAction::Started,
                    ReconcileReason::StartedTentative { exit_code: exit },
                )
            }
        }
    }
}

/// Execute the start side: probe binds for ESTALE, ask the breaker
/// (if tentative), call `adapter.start()` unless dry. Returns the
/// final (possibly downgraded) row.
async fn run_start_pipeline(
    adapter: &dyn RuntimeAdapter,
    container: &Container,
    probe: &dyn MountProbe,
    breaker_store: &dyn BreakerStore,
    dispatcher: Option<&Dispatcher>,
    dry_run: bool,
    start_errors: &mut Vec<StartFailure>,
) -> ReconcileRow {
    // Stale-mount gate — runs BEFORE the breaker gate, intentionally:
    // a container with a stale bind source can never start successfully,
    // and arming the breaker on every such tick would burn the
    // sliding-window budget on a failure mode the breaker is not
    // designed to remediate. The trade-off (the breaker stays dormant
    // for stale-mount-blocked containers until the mount recovers) is
    // accepted per [[project-breaker-followup-6-bear-punch-list]] #10.
    let mut blocked_sources: Vec<PathBuf> = Vec::new();
    for m in &container.mounts {
        if let MountProbeResult::Stale = probe.probe(&m.source) {
            blocked_sources.push(m.source.clone());
        }
    }
    if !blocked_sources.is_empty() {
        emit_stale_mount_blocked(dispatcher, container, &blocked_sources).await;
        return ReconcileRow {
            host: container.host.clone(),
            runtime: container.runtime,
            id: container.id.clone(),
            name: container.name.clone(),
            action: ReconcileAction::BlockedStaleMount,
            reason: ReconcileReason::StaleMount { blocked_sources },
        };
    }

    // Breaker gate. The breaker classifies crashloop signals (docker
    // restart-storm / fast re-exit, lxc flapping / journal failures)
    // and short-circuits to `containers.held_pending_breaker` when it
    // trips.
    //
    // Whether to arm here is runtime-dispatched:
    //
    // * **docker** — only arm on a tentative start (`Exited` with a
    //   non-zero exit code). Clean exits and live containers don't go
    //   through this pipeline, and `fold_docker` relies on
    //   `restart_count` deltas which the start-intent already captures.
    // * **lxc** — `run_start_pipeline` is entered when classify decided
    //   to start (state ∈ {Created, Exited, Dead}). Always arm. LXC
    //   adapters don't surface `exit_code`, so the docker-style "exited
    //   non-zero" gate would dead-letter every LXC start and leave all
    //   the observation/journal/transition plumbing dormant. Per-tick
    //   observation for *running* LXC happens inline in the reconcile
    //   dispatch loop's `NoOp` branch (search for `arm_on_every_start`
    //   in [`reconcile`]); this arm is the "we're about to issue a
    //   start" arm.
    // * **podman / nspawn** — no classifier yet; skip.
    //
    // Dry runs skip the gate entirely — arming would mutate the
    // persisted record (push `recent_starts`, refresh
    // `restart_count_snapshot`), and dry mode is contractually
    // read-only at this layer.
    //
    // `adapter.observe(container)` is the per-tick observation hook:
    // docker returns `HostObservation::default()` (its classifier
    // works from `Container.restart_count` alone), lxc runs
    // `journalctl -u pve-container@<vmid>.service` for the journal
    // tail. Cross-tick `lxc_previous_state` is owned by the breaker
    // (persisted in `BreakerRecord::last_observed_state`).
    let tentative = matches!(container.state, ContainerState::Exited)
        && !matches!(container.exit_code, None | Some(0));
    // Per-runtime arming policy lives in `breaker::arm_on_every_start`
    // (see that function's doc for the full rationale). Runtimes that
    // don't surface `exit_code` — LXC today — need every-start arming
    // because the docker-style tentative gate dead-letters them.
    let should_arm = !dry_run && (tentative || breaker::arm_on_every_start(container.runtime));

    if should_arm
        && arm_and_dispatch_hold(
            adapter,
            container,
            breaker_store,
            dispatcher,
            /* initiating_start */ true,
        )
        .await
        .is_some()
    {
        return ReconcileRow {
            host: container.host.clone(),
            runtime: container.runtime,
            id: container.id.clone(),
            name: container.name.clone(),
            action: ReconcileAction::HeldPendingBreaker,
            reason: ReconcileReason::BreakerHeld {
                exit_code: container.exit_code,
            },
        };
    }

    // Execute the start (unless dry).
    let mut start_failed = false;
    if !dry_run && let Err(e) = adapter.start(&container.id).await {
        start_failed = true;
        start_errors.push(StartFailure {
            host: container.host.clone(),
            runtime: container.runtime,
            id: container.id.clone(),
            name: container.name.clone(),
            message: e.to_string(),
        });
    }

    // Emit `containers.started` only for actual (non-dry) starts that the
    // adapter accepted. A failed start is captured in `start_errors`;
    // telling operators the container "started" when the call errored is a
    // false positive. Dry runs produce the same plan rows but no
    // notifications — the dispatcher's job is to tell operators what
    // *happened*, not what *would* happen.
    if !dry_run && !start_failed {
        emit_started(dispatcher, container, tentative).await;
    }

    let reason = if tentative {
        ReconcileReason::StartedTentative {
            exit_code: container.exit_code,
        }
    } else {
        ReconcileReason::StartedClean
    };
    ReconcileRow {
        host: container.host.clone(),
        runtime: container.runtime,
        id: container.id.clone(),
        name: container.name.clone(),
        action: ReconcileAction::Started,
        reason,
    }
}

/// Arm the breaker + handle a `Hold` by emitting a notification with
/// per-hold suppression. Shared between the start-pipeline (where Hold
/// returns a `HeldPendingBreaker` row) and the observe-only path
/// (where Hold just notifies — a currently-running container can't be
/// unwound, but the operator should know the next start will block).
///
/// Returns `Some(reason)` on Hold, `None` on Proceed or a recoverable
/// store error. Storage errors degrade to Proceed per
/// [[feedback-no-hiding-errors]] — refusing to act because the breaker
/// store hiccuped would itself be a new class of outage.
async fn arm_and_dispatch_hold(
    adapter: &dyn RuntimeAdapter,
    container: &Container,
    breaker_store: &dyn BreakerStore,
    dispatcher: Option<&Dispatcher>,
    initiating_start: bool,
) -> Option<HoldReason> {
    let observation = adapter.observe(container).await;
    let decision = match breaker::arm(ArmRequest {
        container,
        observation: &observation,
        now: utils::time::now(),
        store: breaker_store,
        initiating_start,
    }) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                container = %container.name,
                host = %container.host,
                error = %e,
                initiating_start,
                "breaker arm failed; treating as Proceed"
            );
            BreakerDecision::Proceed
        }
    };

    let BreakerDecision::Hold {
        reason,
        notified_at,
    } = decision
    else {
        return None;
    };

    // Suppress repeat notifications for the same hold. `notified_at` rides
    // back on the decision so we don't re-`load()` the record we just
    // wrote — set by `mark_notified` on a prior tick, cleared by `unhold`.
    let already_notified = notified_at.is_some();
    if !already_notified {
        // The observe-only call site passes `initiating_start=false` (the
        // container is currently running; the hold takes effect on the
        // next start). The start-pipeline passes `true` (the hold blocked
        // an in-flight start). That 1:1 maps to `currently_running`.
        let currently_running = !initiating_start;
        let outcomes =
            emit_held_pending_breaker(dispatcher, container, &reason, currently_running).await;
        let any_ok = outcomes.iter().any(|o| o.result.is_ok());
        if any_ok
            && let Err(e) = breaker::mark_notified(
                breaker_store,
                &container.host,
                container.runtime,
                &container.id,
                utils::time::now(),
            )
        {
            tracing::warn!(
                container = %container.name,
                host = %container.host,
                error = %e,
                "mark_notified failed; alert may repeat next tick"
            );
        }
    }
    Some(reason)
}

// ── Event emission helpers ───────────────────────────────────────────────
//
// Each helper translates a typed payload into an `Event` body and
// dispatches it. The dispatcher's job is backend routing; ours is
// payload shape. We render the typed payload into the `body` field as
// human-readable text plus stuff the payload into the event's `source`
// + `host` + `title` slots so downstream backends can render it
// however they want.

async fn emit_started(dispatcher: Option<&Dispatcher>, container: &Container, tentative: bool) {
    let Some(d) = dispatcher else { return };
    let payload = StartedPayload {
        host: container.host.clone(),
        runtime: container.runtime,
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        restart_policy: container.restart_policy,
        exit_code: container.exit_code,
        tentative,
    };
    let event = Event::new(
        EventClass::Lifecycle,
        Severity::Info,
        format!("containers.started: {}", payload.container_name),
        "reconciler:containers",
    )
    .with_host(payload.host.clone())
    .with_body(render_started_body(&payload));
    let _outcomes = d.emit(&event).await;
}

async fn emit_skipped_policy(dispatcher: Option<&Dispatcher>, container: &Container) {
    let Some(d) = dispatcher else { return };
    let payload = SkippedPolicyPayload {
        host: container.host.clone(),
        runtime: container.runtime,
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        restart_policy: container.restart_policy,
        state: container.state,
    };
    // Lifecycle + Info because there is no `Debug` severity on the
    // notifications ladder yet; debug-equivalent semantics are
    // expressed via class. Routing config filters these out by
    // default — see notifications.md §9.3.
    let event = Event::new(
        EventClass::Lifecycle,
        Severity::Info,
        format!("containers.skipped_policy: {}", payload.container_name),
        "reconciler:containers",
    )
    .with_host(payload.host.clone())
    .with_body(render_skipped_policy_body(&payload));
    let _outcomes = d.emit(&event).await;
}

async fn emit_skipped_label(
    dispatcher: Option<&Dispatcher>,
    container: &Container,
    row: &ReconcileRow,
) {
    let Some(d) = dispatcher else { return };
    let reason = match &row.reason {
        ReconcileReason::LabelOverride { reason } => *reason,
        // Defensive — classify() guarantees this branch alignment but
        // a future caller-side mistake should not silently emit the
        // wrong event. We pick Skip as the conservative default and
        // log a tracing event so the discrepancy is visible.
        _ => {
            tracing::warn!(
                container = %container.name,
                "emit_skipped_label called with non-LabelOverride reason"
            );
            SkipLabelReason::Skip
        }
    };
    let payload = SkippedLabelPayload {
        host: container.host.clone(),
        runtime: container.runtime,
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        reason,
    };
    let event = Event::new(
        EventClass::Lifecycle,
        Severity::Info,
        format!("containers.skipped_label: {}", payload.container_name),
        "reconciler:containers",
    )
    .with_host(payload.host.clone())
    .with_body(render_skipped_label_body(&payload));
    let _outcomes = d.emit(&event).await;
}

async fn emit_stale_mount_blocked(
    dispatcher: Option<&Dispatcher>,
    container: &Container,
    blocked_sources: &[PathBuf],
) {
    let Some(d) = dispatcher else { return };
    let payload = StaleMountBlockedPayload {
        host: container.host.clone(),
        runtime: container.runtime,
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        blocked_sources: blocked_sources.to_vec(),
    };
    let event = Event::new(
        EventClass::Drift,
        Severity::Warn,
        format!(
            "containers.start_blocked_stale_mount: {}",
            payload.container_name
        ),
        "reconciler:containers",
    )
    .with_host(payload.host.clone())
    .with_body(render_stale_mount_blocked_body(&payload));
    let _outcomes = d.emit(&event).await;
}

/// Returns the dispatcher outcomes so the caller can decide whether
/// to stamp `notified_at` (only when at least one backend accepted
/// the event). Returns an empty Vec when there is no dispatcher — no
/// attempt, no stamp.
async fn emit_held_pending_breaker(
    dispatcher: Option<&Dispatcher>,
    container: &Container,
    hold_reason: &HoldReason,
    currently_running: bool,
) -> Vec<EmitOutcome> {
    let Some(d) = dispatcher else {
        return Vec::new();
    };
    let payload = HeldPendingBreakerPayload {
        host: container.host.clone(),
        runtime: container.runtime,
        container_id: container.id.clone(),
        container_name: container.name.clone(),
        exit_code: container.exit_code,
        hold_reason: hold_reason.clone(),
        currently_running,
    };
    let event = Event::new(
        EventClass::Alert,
        Severity::Warn,
        format!(
            "containers.held_pending_breaker: {}",
            payload.container_name
        ),
        "reconciler:containers",
    )
    .with_host(payload.host.clone())
    .with_body(render_held_pending_breaker_body(&payload));
    d.emit(&event).await
}

fn render_started_body(p: &StartedPayload) -> String {
    let mut s = format!(
        "started container `{}` ({}) on `{}` — policy: {:?}",
        p.container_name,
        p.runtime.as_str(),
        p.host,
        p.restart_policy
    );
    if p.tentative {
        s.push_str(", tentative restart after non-zero exit");
        if let Some(code) = p.exit_code {
            s.push_str(&format!(" (exit code {code})"));
        }
    }
    s
}

fn render_skipped_policy_body(p: &SkippedPolicyPayload) -> String {
    format!(
        "skipped `{}` ({}) on `{}`: restart policy {:?} is not auto-start, state {:?}",
        p.container_name,
        p.runtime.as_str(),
        p.host,
        p.restart_policy,
        p.state
    )
}

fn render_skipped_label_body(p: &SkippedLabelPayload) -> String {
    let reason = match p.reason {
        SkipLabelReason::Skip => "orca.skip=true",
        SkipLabelReason::Manual => "orca.heal=manual",
    };
    format!(
        "skipped `{}` ({}) on `{}`: label {reason}",
        p.container_name,
        p.runtime.as_str(),
        p.host
    )
}

fn render_stale_mount_blocked_body(p: &StaleMountBlockedPayload) -> String {
    let sources = p
        .blocked_sources
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "BLOCKED start of `{}` ({}) on `{}` — stale mount sources: {sources}",
        p.container_name,
        p.runtime.as_str(),
        p.host
    )
}

fn render_held_pending_breaker_body(p: &HeldPendingBreakerPayload) -> String {
    let code = p
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".to_string());
    let reason = match &p.hold_reason {
        HoldReason::RestartStormIn5Min { count, .. } => {
            format!("restart storm: {count} starts in 5 min")
        }
        HoldReason::FastReexitAfterOrcaStart {
            within_secs,
            exit_code,
        } => format!("fast re-exit: exit {exit_code} within {within_secs}s of orca-issued start"),
        HoldReason::LxcFlappingIn5Min { transitions, .. } => {
            format!("lxc flapping: {transitions} state transitions in 5 min")
        }
        HoldReason::LxcJournalFailuresIn5Min { count, .. } => {
            format!("lxc journal: {count} failure lines in 5 min")
        }
    };
    let prefix = if p.currently_running {
        "HELD next start of"
    } else {
        "HELD start of"
    };
    format!(
        "{prefix} `{}` ({}) on `{}` — breaker open ({reason}), last exit code {code}",
        p.container_name,
        p.runtime.as_str(),
        p.host
    )
}

// ── Tool surface ─────────────────────────────────────────────────────────

/// Arguments for `containers.reconcile` / `containers.reconcile_dry`.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainersReconcileArgs {
    /// Restrict to one runtime (`docker`, `lxc`, `podman`, `nspawn`).
    /// Defaults to all registered adapters.
    #[arg(long)]
    pub runtime: Option<String>,
}

/// Execute one reconcile pass across every registered adapter.
///
// TODO(C-series): wire into orca scheduler — see
// project_polling_rate_too_slow.md for cadence requirements. The
// scheduler calls `containers.reconcile` — that's the unit.
#[derive::orca_tool(domain = "containers", verb = "reconcile", crate = ::macro_runtime)]
async fn containers_reconcile(
    args: ContainersReconcileArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ReconcileOutput> {
    let adapters = filtered_adapters(args.runtime.as_deref());
    let probe = RealMountProbe;
    let dispatcher: Option<&Dispatcher> = None;
    let breaker_store = default_breaker_store();
    Ok(reconcile(ReconcileInput {
        adapters,
        probe: &probe,
        dispatcher,
        breaker_store: breaker_store.as_ref(),
        dry_run: false,
    })
    .await)
}

/// Plan-only sibling of [`containers_reconcile`]: classifies and
/// probes (read-only), never starts and never arms the breaker.
#[derive::orca_tool(domain = "containers", verb = "reconcile_dry", crate = ::macro_runtime)]
async fn containers_reconcile_dry(
    args: ContainersReconcileArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ReconcileOutput> {
    let adapters = filtered_adapters(args.runtime.as_deref());
    let probe = RealMountProbe;
    let dispatcher: Option<&Dispatcher> = None;
    let breaker_store = default_breaker_store();
    Ok(reconcile(ReconcileInput {
        adapters,
        probe: &probe,
        dispatcher,
        breaker_store: breaker_store.as_ref(),
        dry_run: true,
    })
    .await)
}

// ── Tool: containers.unhold ──────────────────────────────────────────────

/// Arguments for `containers.unhold`. All three fields are required —
/// the breaker keys on `(host, runtime, container_id)`, and an
/// operator clearing a hold must know which record they're acting on
/// (the hold message names them).
#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainersUnholdArgs {
    /// Host the held container lives on (matches `BreakerRecord::host`).
    #[arg(long)]
    pub host: String,
    /// Runtime kind: one of `docker`, `lxc`, `podman`, `nspawn`.
    /// String at the tool boundary because `RuntimeKind` doesn't
    /// implement `clap::ValueEnum`; parsed via
    /// [`parse_runtime_kind`] inside the tool body.
    #[arg(long)]
    pub runtime: String,
    /// Runtime-native container id (docker id, lxc vmid as a string).
    #[arg(long)]
    pub container_id: String,
}

/// Tool-facing view of a cleared `BreakerRecord`. Flattened to the
/// fields an operator cares about; timestamps are RFC 3339 strings
/// (`Container` uses the same string-projection pattern at
/// `lib.rs:220-240`).
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainersUnholdOutput {
    pub host: String,
    pub runtime: String,
    pub container_id: String,
    /// Status after the unhold — always `"watching"`.
    pub status: String,
    /// `held_since` from the cleared record, RFC 3339. `None` if the
    /// record's prior held_since was unset (shouldn't happen for a
    /// real Held record, but the type doesn't enforce that).
    pub previously_held_since: Option<String>,
}

/// Clear a `Held` breaker record so the reconciler stops short-
/// circuiting starts. Returns the cleared record's identity + new
/// status. Errors with `NotFound` if no record matches, `NotHeld` if
/// the record is in any state other than `Held`.
#[derive::orca_tool(domain = "containers", verb = "unhold", crate = ::macro_runtime)]
async fn containers_unhold(
    args: ContainersUnholdArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ContainersUnholdOutput> {
    let runtime = parse_runtime_kind(&args.runtime)?;
    let store = default_breaker_store();
    let record = breaker::unhold(store.as_ref(), &args.host, runtime, &args.container_id)?;
    Ok(ContainersUnholdOutput {
        host: record.host,
        runtime: record.runtime.as_str().to_string(),
        container_id: record.container_id,
        status: "watching".to_string(),
        previously_held_since: record.held_since.map(|t| t.to_rfc3339()),
    })
}

// ── Tool: containers.unwedge ─────────────────────────────────────────────

/// Arguments for `containers.unwedge`. Mirrors [`ContainersUnholdArgs`]
/// — keying on `(host, runtime, container_id)` so the operator names
/// the same record the wedged-detection event named.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainersUnwedgeArgs {
    /// Proxmox node / docker host the wedged container lives on.
    #[arg(long)]
    pub host: String,
    /// Runtime kind: `docker`, `lxc`, `podman`, or `nspawn`. String at
    /// the tool boundary for the same reason `containers.unhold` uses
    /// one — `RuntimeKind` doesn't implement `clap::ValueEnum`.
    #[arg(long)]
    pub runtime: String,
    /// Runtime-native container id (docker id, lxc vmid as a string).
    #[arg(long)]
    pub container_id: String,
}

/// Outcome of one `containers.unwedge` call. Flat — `Liveness` is
/// projected to strings at the boundary just like `containers.unhold`
/// does for the runtime field.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainersUnwedgeOutput {
    pub host: String,
    pub runtime: String,
    pub container_id: String,
    /// True iff the post-attempt liveness probe came back `Live`.
    pub recovered: bool,
    /// Wall-clock seconds from start of the attempt to post-probe.
    pub attempt_duration_secs: f64,
    /// `live` / `wedged` / `unknown` / `not_applicable` — what
    /// liveness reported after the recovery call returned.
    pub post_probe: String,
    /// Adapter error message when the recovery call itself failed.
    /// `None` when the recovery call returned Ok, regardless of
    /// `recovered`.
    pub error: Option<String>,
}

/// Manually trigger recovery for a wedged container. Routes through
/// the same [`crate::wedge::attempt_unwedge`] free fn the auto-recovery
/// loop will call — one handler, three skins
/// ([[feedback-cli-api-mcp-one-path]]).
///
/// Errors with `NotFound` if no adapter for `runtime` is registered,
/// or if `(host, container_id)` doesn't resolve to a known container.
/// The recovery-attempt outcome — including a failed recovery — comes
/// back via [`ContainersUnwedgeOutput::recovered`] / `error`, never as
/// an `Err`.
#[derive::orca_tool(domain = "containers", verb = "unwedge", crate = ::macro_runtime)]
async fn containers_unwedge(
    args: ContainersUnwedgeArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<ContainersUnwedgeOutput> {
    let runtime = parse_runtime_kind(&args.runtime)?;
    let adapters = registered_adapters();
    let adapter = adapters
        .into_iter()
        .find(|a| a.kind() == runtime)
        .ok_or_else(|| {
            anyhow::anyhow!("no adapter registered for runtime `{}`", runtime.as_str())
        })?;

    // Fetch the container so the recovery call has the typed row
    // (host, labels, etc.). `inspect` errors with NotFound for an
    // unknown id — propagate directly.
    let container = adapter
        .inspect(&args.container_id)
        .await
        .map_err(|e| anyhow::anyhow!("inspect {}: {e}", args.container_id))?;

    let outcome = crate::wedge::attempt_unwedge(adapter.as_ref(), &container)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(ContainersUnwedgeOutput {
        host: args.host,
        runtime: runtime.as_str().to_string(),
        container_id: args.container_id,
        recovered: outcome.recovered,
        attempt_duration_secs: outcome.attempt_duration_secs,
        post_probe: outcome.post_probe.as_str().to_string(),
        error: outcome.error,
    })
}

fn parse_runtime_kind(s: &str) -> anyhow::Result<RuntimeKind> {
    match s.to_ascii_lowercase().as_str() {
        "docker" => Ok(RuntimeKind::Docker),
        "lxc" => Ok(RuntimeKind::Lxc),
        "podman" => Ok(RuntimeKind::Podman),
        "nspawn" => Ok(RuntimeKind::Nspawn),
        other => {
            anyhow::bail!("unknown runtime `{other}`: expected one of docker, lxc, podman, nspawn")
        }
    }
}

/// Pick the right `BreakerStore` for the tool surface: a
/// [`FileStore`] rooted at `<orca_home>/containers` when
/// [`FileStore::default_path`] resolves (the common case — we have
/// either `ORCA_HOME` or `HOME`), otherwise a process-local
/// [`MemoryStore`]. The MemoryStore fallback exists for environments
/// with neither env var (rare; container/CI sandboxes); state lives
/// only for the lifetime of the reconcile call and the breaker re-
/// observes the runtime on the next tick.
///
/// Db-backed persistence is queued behind the plugin-namespaced db
/// primitive — until then, FileStore is the durable substrate.
fn default_breaker_store() -> Box<dyn BreakerStore> {
    match breaker::FileStore::default_path() {
        Some(dir) => Box::new(breaker::FileStore::new(dir)),
        None => {
            tracing::warn!(
                target: "containers::breaker",
                "neither ORCA_HOME nor HOME set; using in-memory breaker store (state lost on restart)"
            );
            Box::new(MemoryStore::new())
        }
    }
}

fn filtered_adapters(runtime: Option<&str>) -> Vec<Arc<dyn RuntimeAdapter>> {
    let all = registered_adapters();
    match runtime {
        None => all,
        Some(want) => {
            let want_lc = want.to_ascii_lowercase();
            all.into_iter()
                .filter(|a| a.kind().as_str() == want_lc)
                .collect()
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AdapterError, ContainerMount, ContainerPort, ContainerState, LogTail, RestartPolicy,
        RuntimeKind,
    };
    use derive::orca_async;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    // ── Test helpers ─────────────────────────────────────────────────

    /// Adapter that returns a canned container list and records every
    /// `.start()` call against the same lock.
    struct FakeAdapter {
        kind: RuntimeKind,
        containers: Mutex<Vec<Container>>,
        started: Mutex<Vec<String>>,
        list_err: Mutex<Option<AdapterError>>,
        start_err: Mutex<HashMap<String, AdapterError>>,
        observation: Mutex<crate::breaker::HostObservation>,
    }

    impl FakeAdapter {
        fn new(kind: RuntimeKind, containers: Vec<Container>) -> Self {
            Self {
                kind,
                containers: Mutex::new(containers),
                started: Mutex::new(Vec::new()),
                list_err: Mutex::new(None),
                start_err: Mutex::new(HashMap::new()),
                observation: Mutex::new(crate::breaker::HostObservation::default()),
            }
        }
        fn started_ids(&self) -> Vec<String> {
            self.started.lock().expect("mutex poisoned").clone()
        }
        fn set_list_error(&self, err: AdapterError) {
            *self.list_err.lock().expect("mutex poisoned") = Some(err);
        }
        fn set_start_error(&self, id: &str, err: AdapterError) {
            self.start_err
                .lock()
                .expect("mutex poisoned")
                .insert(id.to_string(), err);
        }
        /// Test hook — swap the next-tick state observed by `list()` for
        /// `id`. Lets the LXC every-tick observation tests simulate
        /// stopped→running transitions across reconciles.
        fn set_state(&self, id: &str, state: ContainerState) {
            let mut g = self.containers.lock().expect("mutex poisoned");
            for c in g.iter_mut() {
                if c.id == id {
                    c.state = state;
                }
            }
        }
        /// Test hook — override the `HostObservation` returned by
        /// `observe()`. Default is `HostObservation::default()`.
        fn set_observation(&self, obs: crate::breaker::HostObservation) {
            *self.observation.lock().expect("mutex poisoned") = obs;
        }
    }

    #[orca_async]
    impl RuntimeAdapter for FakeAdapter {
        fn kind(&self) -> RuntimeKind {
            self.kind
        }
        async fn list(&self, _f: &ListFilter) -> Result<Vec<Container>, AdapterError> {
            if let Some(e) = self.list_err.lock().expect("mutex poisoned").take() {
                return Err(e);
            }
            Ok(self.containers.lock().expect("mutex poisoned").clone())
        }
        async fn inspect(&self, _id: &str) -> Result<Container, AdapterError> {
            Err(AdapterError::NotFound("inspect not used in tests".into()))
        }
        async fn start(&self, id: &str) -> Result<(), AdapterError> {
            if let Some(e) = self.start_err.lock().expect("mutex poisoned").remove(id) {
                return Err(e);
            }
            self.started
                .lock()
                .expect("mutex poisoned")
                .push(id.to_string());
            Ok(())
        }
        async fn stop(&self, _id: &str) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn restart(&self, _id: &str) -> Result<(), AdapterError> {
            Ok(())
        }
        async fn logs(&self, _id: &str, _tail: LogTail) -> Result<String, AdapterError> {
            Ok(String::new())
        }
        async fn observe(&self, _c: &Container) -> crate::breaker::HostObservation {
            self.observation.lock().expect("mutex poisoned").clone()
        }
    }

    /// Probe that returns canned answers per source.
    struct FakeMountProbe {
        answers: HashMap<PathBuf, MountProbeResult>,
        default: MountProbeResult,
    }

    impl FakeMountProbe {
        fn all_ok() -> Self {
            Self {
                answers: HashMap::new(),
                default: MountProbeResult::Ok,
            }
        }
        fn with(answers: Vec<(PathBuf, MountProbeResult)>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                default: MountProbeResult::Ok,
            }
        }
    }

    impl MountProbe for FakeMountProbe {
        fn probe(&self, source: &Path) -> MountProbeResult {
            self.answers
                .get(source)
                .cloned()
                .unwrap_or_else(|| self.default.clone())
        }
    }

    fn mk(
        name: &str,
        policy: RestartPolicy,
        state: ContainerState,
        exit_code: Option<i32>,
        labels: Vec<(&str, &str)>,
        mounts: Vec<&str>,
    ) -> Container {
        Container {
            id: format!("id-{name}"),
            name: name.to_string(),
            runtime: RuntimeKind::Docker,
            host: "testhost".to_string(),
            state,
            restart_policy: policy,
            image: Some(format!("img/{name}")),
            labels: labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            mounts: mounts
                .into_iter()
                .map(|src| ContainerMount {
                    source: PathBuf::from(src),
                    target: PathBuf::from("/data"),
                    read_only: false,
                })
                .collect(),
            ports: Vec::<ContainerPort>::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code,
            startup: None,
        }
    }

    async fn run(
        adapter: Arc<FakeAdapter>,
        probe: &dyn MountProbe,
        dry_run: bool,
    ) -> ReconcileOutput {
        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter as Arc<dyn RuntimeAdapter>];
        let breaker_store = MemoryStore::new();
        reconcile(ReconcileInput {
            adapters,
            probe,
            dispatcher: None,
            dry_run,
            breaker_store: &breaker_store,
        })
        .await
    }

    fn one_row(out: &ReconcileOutput) -> &ReconcileRow {
        assert_eq!(out.rows.len(), 1, "expected one row, got {:?}", out.rows);
        &out.rows[0]
    }

    // ── Decision table ───────────────────────────────────────────────
    //
    // The decision table, expanded to every restart_policy × state × label combination
    // the reconciler can encounter.

    #[tokio::test]
    async fn unless_stopped_created_starts() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::UnlessStopped,
                ContainerState::Created,
                None,
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
        assert_eq!(one_row(&out).reason, ReconcileReason::StartedClean);
        assert_eq!(a.started_ids(), vec!["id-c1"]);
    }

    #[tokio::test]
    async fn always_dead_starts() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Dead,
                None,
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
        assert_eq!(a.started_ids(), vec!["id-c1"]);
    }

    #[tokio::test]
    async fn unless_stopped_exited_clean_starts() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::UnlessStopped,
                ContainerState::Exited,
                Some(0),
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
        assert_eq!(one_row(&out).reason, ReconcileReason::StartedClean);
        assert_eq!(a.started_ids(), vec!["id-c1"]);
    }

    #[tokio::test]
    async fn unless_stopped_exited_nonzero_tentative_proceeds() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::UnlessStopped,
                ContainerState::Exited,
                Some(137),
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        let row = one_row(&out);
        assert_eq!(row.action, ReconcileAction::Started);
        assert_eq!(
            row.reason,
            ReconcileReason::StartedTentative {
                exit_code: Some(137)
            }
        );
        // C3 stub breaker always Proceeds → start is executed.
        assert_eq!(a.started_ids(), vec!["id-c1"]);
    }

    #[tokio::test]
    async fn always_exited_no_exit_code_treated_as_clean() {
        // Defensive: an adapter that doesn't surface exit_code (e.g.
        // lxc adapter today) must not get classified tentative.
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Exited,
                None,
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).reason, ReconcileReason::StartedClean);
    }

    #[tokio::test]
    async fn policy_no_skips_with_policy_reason() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::No,
                ContainerState::Exited,
                Some(0),
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        let row = one_row(&out);
        assert_eq!(row.action, ReconcileAction::SkippedPolicy);
        assert_eq!(
            row.reason,
            ReconcileReason::PolicyNotAutoStart {
                policy: RestartPolicy::No
            }
        );
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn policy_on_failure_skips_with_policy_reason() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::OnFailure,
                ContainerState::Dead,
                Some(2),
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::SkippedPolicy);
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn running_container_is_noop_regardless_of_policy() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![
                mk(
                    "c1",
                    RestartPolicy::UnlessStopped,
                    ContainerState::Running,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "c2",
                    RestartPolicy::No,
                    ContainerState::Running,
                    None,
                    vec![],
                    vec![],
                ),
            ],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        for row in &out.rows {
            assert_eq!(row.action, ReconcileAction::NoOp, "row: {row:?}");
        }
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn unknown_state_is_noop_even_when_desired_running() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Unknown,
                None,
                vec![],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::NoOp);
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn paused_and_stopping_are_noop() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![
                mk(
                    "c1",
                    RestartPolicy::Always,
                    ContainerState::Paused,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "c2",
                    RestartPolicy::UnlessStopped,
                    ContainerState::Stopping,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "c3",
                    RestartPolicy::Always,
                    ContainerState::Starting,
                    None,
                    vec![],
                    vec![],
                ),
            ],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        for row in &out.rows {
            assert_eq!(row.action, ReconcileAction::NoOp);
        }
    }

    // ── Label overrides ──────────────────────────────────────────────

    #[tokio::test]
    async fn skip_label_beats_unless_stopped_and_running_state() {
        // orca.skip wins over policy AND over state.
        let cases = vec![
            ContainerState::Created,
            ContainerState::Exited,
            ContainerState::Dead,
            ContainerState::Running,
        ];
        for state in cases {
            let a = Arc::new(FakeAdapter::new(
                RuntimeKind::Docker,
                vec![mk(
                    "c1",
                    RestartPolicy::UnlessStopped,
                    state,
                    Some(0),
                    vec![("orca.skip", "true")],
                    vec![],
                )],
            ));
            let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
            let row = one_row(&out);
            assert_eq!(row.action, ReconcileAction::SkippedLabel);
            assert_eq!(
                row.reason,
                ReconcileReason::LabelOverride {
                    reason: SkipLabelReason::Skip
                }
            );
            assert!(a.started_ids().is_empty(), "state={state:?}");
        }
    }

    #[tokio::test]
    async fn heal_manual_label_wins_over_policy() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Exited,
                Some(0),
                vec![("orca.heal", "manual")],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        let row = one_row(&out);
        assert_eq!(row.action, ReconcileAction::SkippedLabel);
        assert_eq!(
            row.reason,
            ReconcileReason::LabelOverride {
                reason: SkipLabelReason::Manual
            }
        );
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn skip_label_takes_precedence_over_heal_manual() {
        // Both labels present — Skip wins (checked first).
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Dead,
                None,
                vec![("orca.skip", "true"), ("orca.heal", "manual")],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(
            one_row(&out).reason,
            ReconcileReason::LabelOverride {
                reason: SkipLabelReason::Skip
            }
        );
    }

    #[tokio::test]
    async fn orca_skip_false_does_not_skip() {
        // Only the exact value `true` qualifies.
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Dead,
                None,
                vec![("orca.skip", "false")],
                vec![],
            )],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
    }

    // ── Stale-mount gate ────────────────────────────────────────────

    #[tokio::test]
    async fn stale_bind_mount_blocks_start_and_records_sources() {
        let bad = PathBuf::from("/mnt/alpha/data");
        let good = PathBuf::from("/mnt/alpha/config");
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "sabnzbd",
                RestartPolicy::UnlessStopped,
                ContainerState::Exited,
                Some(0),
                vec![],
                vec![bad.to_str().expect("path"), good.to_str().expect("path")],
            )],
        ));
        let probe = FakeMountProbe::with(vec![(bad.clone(), MountProbeResult::Stale)]);
        let out = run(a.clone(), &probe, false).await;
        let row = one_row(&out);
        assert_eq!(row.action, ReconcileAction::BlockedStaleMount);
        match &row.reason {
            ReconcileReason::StaleMount { blocked_sources } => {
                assert_eq!(blocked_sources, &vec![bad]);
            }
            other => panic!("expected StaleMount, got {other:?}"),
        }
        assert!(a.started_ids().is_empty());
    }

    #[tokio::test]
    async fn multiple_stale_sources_all_recorded() {
        let bad1 = PathBuf::from("/mnt/a");
        let bad2 = PathBuf::from("/mnt/b");
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Created,
                None,
                vec![],
                vec![bad1.to_str().expect("path"), bad2.to_str().expect("path")],
            )],
        ));
        let probe = FakeMountProbe::with(vec![
            (bad1.clone(), MountProbeResult::Stale),
            (bad2.clone(), MountProbeResult::Stale),
        ]);
        let out = run(a.clone(), &probe, false).await;
        match &one_row(&out).reason {
            ReconcileReason::StaleMount { blocked_sources } => {
                assert_eq!(blocked_sources.len(), 2);
                assert!(blocked_sources.contains(&bad1));
                assert!(blocked_sources.contains(&bad2));
            }
            other => panic!("expected StaleMount, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_estale_mount_error_does_not_block() {
        let weird = PathBuf::from("/mnt/x");
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Created,
                None,
                vec![],
                vec![weird.to_str().expect("path")],
            )],
        ));
        let probe = FakeMountProbe::with(vec![(
            weird,
            MountProbeResult::OtherError { os_error: Some(13) },
        )]);
        let out = run(a.clone(), &probe, false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
        assert_eq!(a.started_ids(), vec!["id-c1"]);
    }

    #[tokio::test]
    async fn container_with_no_mounts_skips_probe() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Dead,
                None,
                vec![],
                vec![],
            )],
        ));
        // Probe that would Stale anything — proves we don't call it on
        // the empty mount list.
        struct EverythingStale;
        impl MountProbe for EverythingStale {
            fn probe(&self, _: &Path) -> MountProbeResult {
                MountProbeResult::Stale
            }
        }
        let out = run(a.clone(), &EverythingStale, false).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
    }

    #[tokio::test]
    async fn real_mount_probe_classifies_estale_via_raw_os_error() {
        // We can't easily produce a real ESTALE on a regular fs, but
        // we can prove the constant matches the libc value via a
        // platform-conditional check.
        #[cfg(target_os = "linux")]
        assert_eq!(ESTALE_RAW, 116);
        // And we can prove a non-existent path goes through the
        // OtherError branch (it'll be ENOENT, not ESTALE).
        let probe = RealMountProbe;
        match probe.probe(Path::new("/definitely/not/here/orca-test")) {
            MountProbeResult::OtherError { os_error } => {
                assert!(os_error.is_some());
                assert_ne!(os_error, Some(ESTALE_RAW));
            }
            other => panic!("expected OtherError, got {other:?}"),
        }
    }

    // ── Dry-run ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_produces_plan_without_starting() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![
                mk(
                    "c1",
                    RestartPolicy::UnlessStopped,
                    ContainerState::Created,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "c2",
                    RestartPolicy::No,
                    ContainerState::Exited,
                    Some(0),
                    vec![],
                    vec![],
                ),
                mk(
                    "c3",
                    RestartPolicy::Always,
                    ContainerState::Exited,
                    Some(137),
                    vec![],
                    vec![],
                ),
            ],
        ));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), true).await;
        assert!(out.dry_run);
        assert_eq!(out.rows.len(), 3);
        // No starts attempted in dry mode.
        assert!(a.started_ids().is_empty());
        // Plan structure still says what would happen.
        let by_name: HashMap<_, _> = out
            .rows
            .iter()
            .map(|r| (r.name.clone(), r.clone()))
            .collect();
        assert_eq!(by_name["c1"].action, ReconcileAction::Started);
        assert_eq!(by_name["c2"].action, ReconcileAction::SkippedPolicy);
        assert_eq!(by_name["c3"].action, ReconcileAction::Started);
    }

    #[tokio::test]
    async fn dry_run_still_runs_stale_probe() {
        let bad = PathBuf::from("/mnt/alpha/data");
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "c1",
                RestartPolicy::Always,
                ContainerState::Created,
                None,
                vec![],
                vec![bad.to_str().expect("path")],
            )],
        ));
        let probe = FakeMountProbe::with(vec![(bad.clone(), MountProbeResult::Stale)]);
        let out = run(a.clone(), &probe, true).await;
        assert_eq!(one_row(&out).action, ReconcileAction::BlockedStaleMount);
        assert!(a.started_ids().is_empty());
    }

    // ── Adapter failures ────────────────────────────────────────────

    #[tokio::test]
    async fn list_failure_does_not_abort_other_adapters() {
        let docker = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![mk(
                "ok",
                RestartPolicy::Always,
                ContainerState::Created,
                None,
                vec![],
                vec![],
            )],
        ));
        let lxc = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![]));
        lxc.set_list_error(AdapterError::Unavailable("pct not on PATH".into()));

        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![docker.clone() as _, lxc.clone() as _];
        let breaker_store = MemoryStore::new();
        let out = reconcile(ReconcileInput {
            adapters,
            probe: &FakeMountProbe::all_ok(),
            dispatcher: None,
            dry_run: false,
            breaker_store: &breaker_store,
        })
        .await;
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].name, "ok");
        assert_eq!(out.adapter_errors.len(), 1);
        assert_eq!(out.adapter_errors[0].runtime, RuntimeKind::Lxc);
    }

    #[tokio::test]
    async fn start_failure_recorded_and_other_rows_continue() {
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![
                mk(
                    "bad",
                    RestartPolicy::Always,
                    ContainerState::Created,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "good",
                    RestartPolicy::Always,
                    ContainerState::Created,
                    None,
                    vec![],
                    vec![],
                ),
            ],
        ));
        a.set_start_error("id-bad", AdapterError::Refused("locked".into()));
        let out = run(a.clone(), &FakeMountProbe::all_ok(), false).await;
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.start_errors.len(), 1);
        assert_eq!(out.start_errors[0].id, "id-bad");
        // Good row went through.
        assert_eq!(a.started_ids(), vec!["id-good"]);
    }

    /// Records every dispatched event so a test can assert which
    /// notifications actually fired.
    struct RecordingBackend {
        captured: Mutex<Vec<notifications::Event>>,
    }

    #[orca_async]
    impl notifications::Backend for RecordingBackend {
        fn name(&self) -> &str {
            "recording"
        }
        async fn emit(
            &self,
            event: &notifications::Event,
        ) -> Result<notifications::MessageRef, notifications::BackendError> {
            self.captured
                .lock()
                .expect("mutex poisoned")
                .push(event.clone());
            Ok(notifications::MessageRef::new("recording", "msg"))
        }
    }

    #[tokio::test]
    async fn failed_start_does_not_emit_started_notification() {
        // A failed `adapter.start()` must be recorded in `start_errors`
        // without firing the `containers.started` success notification —
        // otherwise operators are told a container started when it didn't.
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![
                mk(
                    "bad",
                    RestartPolicy::Always,
                    ContainerState::Created,
                    None,
                    vec![],
                    vec![],
                ),
                mk(
                    "good",
                    RestartPolicy::Always,
                    ContainerState::Created,
                    None,
                    vec![],
                    vec![],
                ),
            ],
        ));
        a.set_start_error("id-bad", AdapterError::Refused("locked".into()));

        let backend = Arc::new(RecordingBackend {
            captured: Mutex::new(Vec::new()),
        });
        struct Forward(Arc<RecordingBackend>);
        #[orca_async]
        impl notifications::Backend for Forward {
            fn name(&self) -> &str {
                self.0.name()
            }
            async fn emit(
                &self,
                e: &notifications::Event,
            ) -> Result<notifications::MessageRef, notifications::BackendError> {
                self.0.emit(e).await
            }
        }
        let dispatcher = Dispatcher::new().with_backend(Box::new(Forward(backend.clone())));
        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![a.clone() as Arc<dyn RuntimeAdapter>];
        let breaker_store = MemoryStore::new();
        let out = reconcile(ReconcileInput {
            adapters,
            probe: &FakeMountProbe::all_ok(),
            dispatcher: Some(&dispatcher),
            dry_run: false,
            breaker_store: &breaker_store,
        })
        .await;

        assert_eq!(out.start_errors.len(), 1);
        assert_eq!(out.start_errors[0].id, "id-bad");

        let started: Vec<String> = backend
            .captured
            .lock()
            .expect("mutex poisoned")
            .iter()
            .filter(|e| e.title.starts_with("containers.started"))
            .map(|e| e.title.clone())
            .collect();
        // Exactly one started notification — for the container that started.
        assert_eq!(
            started,
            vec!["containers.started: good".to_string()],
            "only the successful start should notify"
        );
    }

    // ── Output shape ────────────────────────────────────────────────

    #[tokio::test]
    async fn rows_sorted_by_host_then_name() {
        let mut bravo = mk(
            "bravo",
            RestartPolicy::Always,
            ContainerState::Created,
            None,
            vec![],
            vec![],
        );
        bravo.host = "beta".to_string();
        let mut alpha = mk(
            "alpha",
            RestartPolicy::Always,
            ContainerState::Created,
            None,
            vec![],
            vec![],
        );
        alpha.host = "alpha".to_string();
        let mut zulu = mk(
            "zulu",
            RestartPolicy::Always,
            ContainerState::Created,
            None,
            vec![],
            vec![],
        );
        zulu.host = "alpha".to_string();
        let a = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![bravo, zulu, alpha],
        ));
        let out = run(a, &FakeMountProbe::all_ok(), false).await;
        let order: Vec<_> = out
            .rows
            .iter()
            .map(|r| (r.host.clone(), r.name.clone()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("alpha".to_string(), "alpha".to_string()),
                ("alpha".to_string(), "zulu".to_string()),
                ("beta".to_string(), "bravo".to_string()),
            ]
        );
    }

    // ── Breaker wiring (C4) ─────────────────────────────────────────
    //
    // The Proceed path is covered by `unless_stopped_exited_nonzero_
    // tentative_proceeds` above (fresh store → no trip → start). These
    // tests cover the Hold path: short-circuit + typed notification +
    // reason round-trip into the rendered body. We pre-seed a
    // `BreakerRecord` with `status=Held` rather than driving the
    // classifier — `arm()` shortcuts on a sticky hold (breaker.rs:500),
    // which is exactly the production behaviour after a previous trip.

    use crate::breaker::{BreakerRecord, BreakerStatus, HoldReason};
    use notifications::{Backend, BackendError, Dispatcher, Event, MessageRef};

    struct CapturingBackend {
        captured: Arc<Mutex<Vec<Event>>>,
    }

    #[orca_async]
    impl Backend for CapturingBackend {
        fn name(&self) -> &str {
            "capturing"
        }
        async fn emit(&self, event: &Event) -> Result<MessageRef, BackendError> {
            self.captured
                .lock()
                .expect("capturing backend mutex poisoned")
                .push(event.clone());
            Ok(MessageRef::new("capturing", "msg-1"))
        }
    }

    /// Pre-seed a `MemoryStore` with a stuck-Held record matching
    /// `container`. `arm()` will short-circuit to `Hold { reason }` on
    /// the next tick without re-running the classifier.
    fn seed_held(store: &MemoryStore, container: &Container, reason: HoldReason) {
        let mut record = BreakerRecord::fresh(&container.host, container.runtime, &container.id);
        record.status = BreakerStatus::Held;
        record.held_reason = Some(reason);
        record.held_since = Some(utils::time::now());
        store.save(&record).expect("seed breaker record");
    }

    #[tokio::test]
    async fn breaker_hold_short_circuits_tentative_start() {
        let container = mk(
            "looper",
            RestartPolicy::UnlessStopped,
            ContainerState::Exited,
            Some(137),
            vec![],
            vec![],
        );
        let adapter = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![container.clone()],
        ));
        let probe = FakeMountProbe::all_ok();
        let breaker_store = MemoryStore::new();
        seed_held(
            &breaker_store,
            &container,
            HoldReason::FastReexitAfterOrcaStart {
                within_secs: 12,
                exit_code: 137,
            },
        );

        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter.clone() as _];
        let out = reconcile(ReconcileInput {
            adapters,
            probe: &probe,
            dispatcher: None,
            dry_run: false,
            breaker_store: &breaker_store,
        })
        .await;

        let row = one_row(&out);
        assert_eq!(row.action, ReconcileAction::HeldPendingBreaker);
        assert_eq!(
            row.reason,
            ReconcileReason::BreakerHeld {
                exit_code: Some(137)
            }
        );
        assert!(
            adapter.started_ids().is_empty(),
            "Hold must not start the container, got: {:?}",
            adapter.started_ids()
        );
    }

    #[tokio::test]
    async fn breaker_hold_emits_held_pending_breaker_notification_with_reason() {
        let container = mk(
            "stormy",
            RestartPolicy::UnlessStopped,
            ContainerState::Exited,
            Some(1),
            vec![],
            vec![],
        );
        let adapter = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![container.clone()],
        ));
        let probe = FakeMountProbe::all_ok();
        let breaker_store = MemoryStore::new();
        let window_start = utils::time::now().minus(std::time::Duration::from_secs(3 * 60));
        seed_held(
            &breaker_store,
            &container,
            HoldReason::RestartStormIn5Min {
                count: 7,
                window_start,
            },
        );

        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Dispatcher::new().with_backend(Box::new(CapturingBackend {
            captured: Arc::clone(&captured),
        }));

        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter as _];
        let _out = reconcile(ReconcileInput {
            adapters,
            probe: &probe,
            dispatcher: Some(&dispatcher),
            dry_run: false,
            breaker_store: &breaker_store,
        })
        .await;

        let events = captured
            .lock()
            .expect("capturing backend mutex poisoned")
            .clone();
        assert_eq!(events.len(), 1, "expected exactly one notification");
        let event = &events[0];
        assert!(
            event.title.starts_with("containers.held_pending_breaker:"),
            "title was {:?}",
            event.title
        );
        assert!(
            event.title.ends_with("stormy"),
            "title did not name the container: {:?}",
            event.title
        );
        let body = &event.body;
        assert!(
            body.contains("restart storm"),
            "body missing classifier reason text: {body}"
        );
        assert!(
            body.contains("7 starts"),
            "body missing classifier count: {body}"
        );
    }

    // ── mark_notified suppression ────────────────────────────────────

    struct FailingBackend;
    #[orca_async]
    impl Backend for FailingBackend {
        fn name(&self) -> &str {
            "failing"
        }
        async fn emit(&self, _event: &Event) -> Result<MessageRef, BackendError> {
            Err(BackendError::Transport("simulated".into()))
        }
    }

    /// Build a fresh adapter+probe+held-store fixture for the
    /// repeat-suppression tests. Container is `name`, runtime
    /// Docker, tentative-eligible (Exited + non-zero exit_code).
    /// Store is pre-seeded with a stuck Hold so each `reconcile`
    /// call routes through the Hold branch.
    fn held_fixture(name: &str) -> (Arc<FakeAdapter>, MemoryStore, Container) {
        let container = mk(
            name,
            RestartPolicy::UnlessStopped,
            ContainerState::Exited,
            Some(1),
            vec![],
            vec![],
        );
        let adapter = Arc::new(FakeAdapter::new(
            RuntimeKind::Docker,
            vec![container.clone()],
        ));
        let store = MemoryStore::new();
        seed_held(
            &store,
            &container,
            HoldReason::FastReexitAfterOrcaStart {
                within_secs: 5,
                exit_code: 1,
            },
        );
        (adapter, store, container)
    }

    async fn run_once_with(
        adapter: &Arc<FakeAdapter>,
        store: &MemoryStore,
        dispatcher: Option<&Dispatcher>,
    ) {
        let probe = FakeMountProbe::all_ok();
        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter.clone() as _];
        let _ = reconcile(ReconcileInput {
            adapters,
            probe: &probe,
            dispatcher,
            dry_run: false,
            breaker_store: store,
        })
        .await;
    }

    #[tokio::test]
    async fn second_held_tick_does_not_re_emit() {
        let (adapter, store, _container) = held_fixture("loop1");
        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Dispatcher::new().with_backend(Box::new(CapturingBackend {
            captured: Arc::clone(&captured),
        }));

        // Tick 1: emits + stamps notified_at.
        run_once_with(&adapter, &store, Some(&dispatcher)).await;
        // Tick 2: notified_at is set → must skip emission.
        run_once_with(&adapter, &store, Some(&dispatcher)).await;

        let events = captured.lock().expect("mutex").clone();
        assert_eq!(
            events.len(),
            1,
            "second tick must not emit while hold is sticky; got events={events:?}"
        );
    }

    #[tokio::test]
    async fn after_unhold_next_trip_emits_again() {
        let (adapter, store, container) = held_fixture("loop2");
        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Dispatcher::new().with_backend(Box::new(CapturingBackend {
            captured: Arc::clone(&captured),
        }));

        // Tick 1: emits once, stamps notified_at.
        run_once_with(&adapter, &store, Some(&dispatcher)).await;
        assert_eq!(captured.lock().expect("mutex").len(), 1);

        // Operator clears the hold.
        crate::breaker::unhold(&store, &container.host, container.runtime, &container.id)
            .expect("unhold");

        // Re-seed the same trip — emulates the next reconciler tick
        // tripping the breaker again.
        seed_held(
            &store,
            &container,
            HoldReason::FastReexitAfterOrcaStart {
                within_secs: 5,
                exit_code: 1,
            },
        );

        // Tick 2: notified_at was cleared by unhold → emit again.
        run_once_with(&adapter, &store, Some(&dispatcher)).await;
        assert_eq!(
            captured.lock().expect("mutex").len(),
            2,
            "post-unhold trip must re-emit"
        );
    }

    #[tokio::test]
    async fn failed_dispatch_does_not_stamp_notified_so_next_tick_retries() {
        let (adapter, store, container) = held_fixture("loop3");
        // First dispatcher: only a failing backend → no successful emit.
        let failing = Dispatcher::new().with_backend(Box::new(FailingBackend));

        // Tick 1: emit attempted, all backends fail → notified_at stays None.
        run_once_with(&adapter, &store, Some(&failing)).await;
        let record_after_tick1 = store
            .load(&container.host, container.runtime, &container.id)
            .expect("load")
            .expect("record");
        assert!(
            record_after_tick1.notified_at.is_none(),
            "failed dispatch must not stamp notified_at"
        );

        // Tick 2: now with a capturing dispatcher → must emit (the
        // retry the dispatch-failure semantic exists for).
        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let ok = Dispatcher::new().with_backend(Box::new(CapturingBackend {
            captured: Arc::clone(&captured),
        }));
        run_once_with(&adapter, &store, Some(&ok)).await;
        assert_eq!(
            captured.lock().expect("mutex").len(),
            1,
            "second tick with a working backend must retry"
        );
        let record_after_tick2 = store
            .load(&container.host, container.runtime, &container.id)
            .expect("load")
            .expect("record");
        assert!(
            record_after_tick2.notified_at.is_some(),
            "successful dispatch must stamp notified_at"
        );
    }

    // ── containers.unhold ────────────────────────────────────────────

    #[test]
    fn parse_runtime_kind_accepts_canonical_names() {
        assert_eq!(parse_runtime_kind("docker").unwrap(), RuntimeKind::Docker);
        assert_eq!(parse_runtime_kind("lxc").unwrap(), RuntimeKind::Lxc);
        assert_eq!(parse_runtime_kind("podman").unwrap(), RuntimeKind::Podman);
        assert_eq!(parse_runtime_kind("nspawn").unwrap(), RuntimeKind::Nspawn);
    }

    #[test]
    fn parse_runtime_kind_is_case_insensitive() {
        assert_eq!(parse_runtime_kind("Docker").unwrap(), RuntimeKind::Docker);
        assert_eq!(parse_runtime_kind("LXC").unwrap(), RuntimeKind::Lxc);
    }

    #[test]
    fn parse_runtime_kind_rejects_unknown() {
        let err = parse_runtime_kind("kvm").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("kvm"), "error must echo input: {msg}");
        assert!(
            msg.contains("docker"),
            "error must list valid options: {msg}"
        );
    }

    // ── LXC every-tick observation ───────────────────────────────────
    //
    // C4-followup #5 (project_session_handoff_2026_06_13_breaker_followups):
    // LXC adapters surface `exit_code: None`, which means the docker-
    // style `Exited && exit_code != 0` gate would never fire and all the
    // breaker plumbing (journal tail, cross-tick state) would sit
    // dormant. These tests pin the corrected behavior:
    //
    // 1. Running LXC with auto-restart → NoOp row, but the breaker is
    //    armed observe-only so `last_observed_state` is captured for the
    //    next tick.
    // 2. Exited → Running across two ticks counts as one `recent_starts`
    //    entry, contributed by `fold_lxc`'s transition detection. The
    //    arm-time push is suppressed for LXC; without that suppression
    //    the count would be 2.
    // 3. Observe-only ticks must not touch `last_orca_start_at` — that
    //    timestamp anchors the docker fast-reexit classifier and would
    //    misbehave if observation moved it.
    // 4. A first-contact LXC start (no persisted prev state) must not
    //    double-count itself: arm doesn't push, fold has no prev → 0.
    // 5. Journal failures observed while running must trip the breaker.

    fn mk_lxc(name: &str, state: ContainerState, policy: RestartPolicy) -> Container {
        Container {
            id: format!("id-{name}"),
            name: name.to_string(),
            runtime: RuntimeKind::Lxc,
            host: "host-a".to_string(),
            state,
            restart_policy: policy,
            image: None,
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            // LXC adapters don't surface exit_code; this is the gap that
            // the runtime-dispatched arming gate exists to handle.
            exit_code: None,
            startup: None,
        }
    }

    async fn run_once(
        adapter: &Arc<FakeAdapter>,
        store: &MemoryStore,
        probe: &dyn MountProbe,
    ) -> ReconcileOutput {
        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter.clone() as _];
        reconcile(ReconcileInput {
            adapters,
            probe,
            dispatcher: None,
            dry_run: false,
            breaker_store: store,
        })
        .await
    }

    #[tokio::test]
    async fn lxc_running_noop_arms_breaker_and_persists_observed_state() {
        // Running LXC with an auto-restart policy. classify() → NoOp,
        // but the dispatch loop's NoOp arm should still call the breaker
        // observe-only so the cross-tick state is laid down for the
        // next tick's fold_lxc.
        let c = mk_lxc("ct100", ContainerState::Running, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();

        let out = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        assert_eq!(one_row(&out).action, ReconcileAction::NoOp);
        // Adapter.start was never called — observation must not start.
        assert!(adapter.started_ids().is_empty(), "must not start a Running");

        let r = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("record must exist — NoOp branch armed breaker");
        assert_eq!(r.status, BreakerStatus::Watching);
        assert_eq!(r.last_observed_state, Some(ContainerState::Running));
    }

    #[tokio::test]
    async fn docker_running_noop_does_not_arm_breaker() {
        // Docker container at Running with auto-restart. classify() →
        // NoOp. The observe-only arm path is gated to runtimes for which
        // `arm_on_every_start` is true (LXC today); docker must NOT arm
        // here — the breaker is only engaged on tentative starts via
        // `run_start_pipeline`.
        let c = mk(
            "dk1",
            RestartPolicy::Always,
            ContainerState::Running,
            None,
            vec![],
            vec![],
        );
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Docker, vec![c.clone()]));
        let store = MemoryStore::new();

        let out = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        assert_eq!(one_row(&out).action, ReconcileAction::NoOp);
        assert!(
            store
                .load(&c.host, c.runtime, &c.id)
                .expect("load")
                .is_none(),
            "docker observe-only must not create a breaker record"
        );
    }

    #[tokio::test]
    async fn lxc_running_noop_dry_run_skips_arm() {
        // Dry-run must leave the breaker store untouched on the
        // observe-only path, just like it leaves the start-pipeline arm
        // untouched. Mirror of the existing dry-run start-pipeline guard.
        let c = mk_lxc("ct200", ContainerState::Running, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();

        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter.clone() as _];
        let _ = reconcile(ReconcileInput {
            adapters,
            probe: &FakeMountProbe::all_ok(),
            dispatcher: None,
            dry_run: true,
            breaker_store: &store,
        })
        .await;

        assert!(
            store
                .load(&c.host, c.runtime, &c.id)
                .expect("load")
                .is_none(),
            "dry_run must not write to the breaker store"
        );
    }

    #[tokio::test]
    async fn lxc_noop_held_emits_once_then_suppresses() {
        // Seeded Hold on a Running LXC. The observe-only NoOp branch
        // arms the breaker, sees the sticky Hold, dispatches the
        // notification, stamps `notified_at`. The second tick sees the
        // sticky Hold with `notified_at` populated and must NOT re-emit.
        let c = mk_lxc("ct201", ContainerState::Running, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();
        seed_held(
            &store,
            &c,
            HoldReason::LxcFlappingIn5Min {
                transitions: 9,
                window_start: utils::time::now().minus(std::time::Duration::from_secs(120)),
            },
        );

        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Dispatcher::new().with_backend(Box::new(CapturingBackend {
            captured: Arc::clone(&captured),
        }));

        // Tick 1: emit + stamp.
        let adapters: Vec<Arc<dyn RuntimeAdapter>> = vec![adapter.clone() as _];
        let _ = reconcile(ReconcileInput {
            adapters: adapters.clone(),
            probe: &FakeMountProbe::all_ok(),
            dispatcher: Some(&dispatcher),
            dry_run: false,
            breaker_store: &store,
        })
        .await;
        // Tick 2: suppress.
        let _ = reconcile(ReconcileInput {
            adapters,
            probe: &FakeMountProbe::all_ok(),
            dispatcher: Some(&dispatcher),
            dry_run: false,
            breaker_store: &store,
        })
        .await;

        let events = captured.lock().expect("mutex").clone();
        assert_eq!(
            events.len(),
            1,
            "observe-only Held must emit once across two ticks; got events={events:?}"
        );
    }

    #[tokio::test]
    async fn lxc_exited_then_running_transition_counted_once_via_fold_lxc() {
        // Two ticks, same container, state changes between them.
        // Tick 1: Exited → run_start_pipeline arms with
        //   initiating_start=true. No prev state, fold sees no
        //   transition, arm suppresses its LXC push → recent_starts == 0.
        // Tick 2: Running → NoOp arms observe-only. fold_lxc reads
        //   persisted prev=Exited from the record overlay, sees
        //   Exited→Running, pushes once → recent_starts == 1.
        // The single push proves we count exactly one start despite
        // arm firing twice across the transition.
        let c = mk_lxc(
            "ct101",
            ContainerState::Exited,
            RestartPolicy::UnlessStopped,
        );
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();

        let _ = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        let r1 = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("tick1 record");
        assert_eq!(
            r1.recent_starts.len(),
            0,
            "tick1 fresh contact: no prev → no fold push, LXC arm doesn't push"
        );
        assert_eq!(r1.last_observed_state, Some(ContainerState::Exited));

        adapter.set_state(&c.id, ContainerState::Running);
        let _ = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        let r2 = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("tick2 record");
        assert_eq!(
            r2.recent_starts.len(),
            1,
            "tick2 transition: fold_lxc pushes exactly one start"
        );
        assert_eq!(r2.last_observed_state, Some(ContainerState::Running));
    }

    #[tokio::test]
    async fn lxc_observe_only_preserves_last_orca_start_at() {
        // Pre-seed a record with last_orca_start_at set. Reconcile a
        // running LXC — observe-only path. The timestamp must survive:
        // moving it would invalidate the docker fast-reexit anchor (LXC
        // doesn't use it today, but the invariant must hold for the
        // shared arm() impl regardless of runtime).
        let c = mk_lxc("ct102", ContainerState::Running, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();

        let mut seed = breaker::BreakerRecord::fresh(&c.host, c.runtime, &c.id);
        let anchor = utils::time::now().minus(std::time::Duration::from_secs(120));
        seed.last_orca_start_at = Some(anchor);
        store.save(&seed).expect("seed");

        let _ = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        let r = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("record");
        assert_eq!(
            r.last_orca_start_at,
            Some(anchor),
            "observe-only must not move last_orca_start_at"
        );
    }

    #[tokio::test]
    async fn lxc_initiating_start_does_not_push_recent_starts() {
        // Regression: pre-#5 arm() unconditionally pushed recent_starts
        // on Proceed. For LXC that double-counts with fold_lxc's
        // next-tick transition push. First-contact LXC start with no
        // prev state must produce zero pushes — the next tick's
        // transition is the canonical source of "we started."
        let c = mk_lxc("ct103", ContainerState::Exited, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        let store = MemoryStore::new();

        let out = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        assert_eq!(one_row(&out).action, ReconcileAction::Started);
        let r = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("record");
        assert!(
            r.recent_starts.is_empty(),
            "LXC arm must not push recent_starts (fold_lxc owns the count): got {:?}",
            r.recent_starts
        );
        // But last_orca_start_at SHOULD be set — that's the
        // initiating_start=true signal.
        assert!(
            r.last_orca_start_at.is_some(),
            "initiating_start=true must stamp last_orca_start_at"
        );
    }

    #[tokio::test]
    async fn lxc_journal_failures_during_running_trip_breaker() {
        // Per breaker::LXC_JOURNAL_FAILURE_THRESHOLD = 3, more than 3
        // "failed to start" / "exited with status" lines in the
        // observed journal tail trips the breaker. Surface this through
        // the reconciler's observe-only NoOp arm — a running container
        // whose journal shows recent flap should be held.
        let c = mk_lxc("ct104", ContainerState::Running, RestartPolicy::Always);
        let adapter = Arc::new(FakeAdapter::new(RuntimeKind::Lxc, vec![c.clone()]));
        adapter.set_observation(crate::breaker::HostObservation {
            // 4 failure lines — strictly greater than the threshold of 3.
            lxc_journal_tail: Some(["Failed to start pve-container@104.service"; 4].join("\n")),
            lxc_previous_state: None,
        });
        let store = MemoryStore::new();

        let out = run_once(&adapter, &store, &FakeMountProbe::all_ok()).await;
        // Row stays NoOp — the container is currently running and the
        // observe-only branch doesn't unwind that. The hold is
        // surfaced via the persisted breaker state and the notification.
        assert_eq!(one_row(&out).action, ReconcileAction::NoOp);

        let r = store
            .load(&c.host, c.runtime, &c.id)
            .expect("load")
            .expect("record");
        assert_eq!(r.status, BreakerStatus::Held);
        assert!(matches!(
            r.held_reason,
            Some(HoldReason::LxcJournalFailuresIn5Min { .. })
        ));
    }
}
