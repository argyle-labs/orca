//! Periodic container-reconcile driver — the scheduler that activates the
//! otherwise-dormant containers reconciler (auto-start + circuit breaker) on a
//! timer, gated by the host [`RemediationPolicy`] so a fleet-wide rollout never
//! surprise-restarts a container.
//!
//! Mirrors [`crate::mount_converge`]: a pure decision core
//! ([`plan_from_policy`], [`actionable_summary`]) plus an async tick that feeds
//! it the reconcile result and raises a dismissable notification when the policy
//! says notify. The reconcile machinery (breaker, wedge recovery, stale-mount
//! gate) already lives in the `containers` crate; this driver only SCHEDULES it
//! and gates its state-changing actions:
//!
//! * `auto_fix`          — reconcile for real (`dry_run=false`); act silently.
//! * `auto_fix_notify`   — reconcile for real; raise a notification of what was done.
//! * `notify`  (default) — reconcile READ-ONLY (`dry_run=true`); NEVER restart;
//!   raise a notification carrying the proposed actions for the operator.
//! * `disabled`          — reconcile read-only; stay silent.
//!
//! The default policy is [`RemediationPolicy::Notify`], so this driver never
//! restarts a container until the operator opts a host into an acting policy —
//! the critical safety property for the first fleet-wide roll.
//!
//! ## Notification plane
//!
//! The reconciler's typed-Event [`notifications::Dispatcher`] plane is not
//! constructed anywhere in the daemon today (the wired path is
//! [`db::notifications_store`]). This driver therefore passes `dispatcher: None`
//! — exactly as the `container.update{action=reconcile}` tool does — and raises
//! a dismissable notification from the returned plan, mirroring the storage
//! converge loop's [`crate::mount_converge`] use of `notifications_store::raise`.
//!
//! ## Wedge auto-recovery
//!
//! The reconciler gates its wedge detection + auto-recovery on a present
//! dispatcher (`!dry_run && dispatcher.is_some()`). Because no `Dispatcher` is
//! constructed in the daemon, wedge auto-recovery stays dormant even under an
//! acting policy; this driver activates auto-start and the crashloop breaker.
//! Wiring the typed-Event dispatcher (or a bridge to `notifications_store`) to
//! unlock wedge recovery is a separate follow-up.

use crate::periodic;
use crate::remediation::{self, RemediationPolicy};
use containers::breaker::BreakerStore;
use containers::reconciler::{
    self, RealMountProbe, ReconcileAction, ReconcileInput, ReconcileOutput,
};
use db::notifications_store::{RaiseInput, Severity};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// `source` the driver's dismissable notifications carry, for scoping.
const NOTIFY_SOURCE: &str = "remediation:containers.reconcile";
/// Stable key for the reconcile-proposal notification (idempotent upsert — a
/// still-actionable condition surfaces as one row, not a per-tick spam).
const NOTIFY_KEY: &str = "remediation:containers:reconcile";

/// Seconds between reconcile ticks. Matches the storage converge cadence.
pub const INTERVAL_SECS: u64 = 30;

/// How a tick should run the reconciler, derived purely from the host policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// True ⇒ read-only pass: the reconciler classifies + probes but takes NO
    /// state-changing action (no start, no wedge recovery, no breaker mutation).
    pub dry_run: bool,
    /// True ⇒ raise a dismissable notification summarizing the actionable rows.
    pub notify: bool,
}

/// Pure policy → run-mode decision. The safety core: only `acts()` policies
/// (`auto_fix` / `auto_fix_notify`) clear `dry_run`; the default `notify` (and
/// `disabled`) run read-only, so a container is never restarted by default.
pub fn plan_from_policy(policy: RemediationPolicy) -> ReconcilePlan {
    ReconcilePlan {
        dry_run: !policy.acts(),
        notify: policy.notifies(),
    }
}

/// Rows that represent a remediation the reconciler took (or, under a read-only
/// pass, WOULD take): a (re)start, a breaker hold, or a stale-mount block. Pure
/// so the notification body is unit-tested; NoOp / skipped rows are omitted.
pub fn actionable_summary(out: &ReconcileOutput) -> Vec<String> {
    out.rows
        .iter()
        .filter(|r| {
            matches!(
                r.action,
                ReconcileAction::Started
                    | ReconcileAction::HeldPendingBreaker
                    | ReconcileAction::BlockedStaleMount
            )
        })
        .map(|r| {
            format!(
                "{} ({}:{}) on {} — {}",
                r.name,
                r.runtime.as_str(),
                r.id,
                r.host,
                action_word(r.action)
            )
        })
        .collect()
}

fn action_word(a: ReconcileAction) -> &'static str {
    match a {
        ReconcileAction::Started => "start",
        ReconcileAction::HeldPendingBreaker => "held (circuit breaker open)",
        ReconcileAction::BlockedStaleMount => "blocked (stale mount)",
        // Non-actionable rows never reach here (filtered out above).
        ReconcileAction::SkippedPolicy | ReconcileAction::SkippedLabel | ReconcileAction::NoOp => {
            ""
        }
    }
}

/// Outcome of one reconcile pass — the raw reconcile result plus whether a
/// notification was raised. Returned so the tick can log it and tests can assert
/// on the policy gate + notification seam.
pub struct PassOutcome {
    pub reconcile: ReconcileOutput,
    pub notified: bool,
}

/// The async half of a pass: run the reconciler under `policy`'s gate against
/// `adapters`, persisting breaker state through `breaker_store`. Takes NO db
/// handle so its future stays `Send` (rusqlite's `Connection` is `!Send`) — the
/// db-touching notify runs synchronously AFTER the await, mirroring the storage
/// converge loop's split.
///
/// `dispatcher` is intentionally `None` (see module docs): the state-changing
/// gate is `dry_run`, and operator-visible feedback is the dismissable
/// notification raised by [`notify_pass`].
pub async fn reconcile_pass(
    policy: RemediationPolicy,
    adapters: Vec<Arc<dyn containers::RuntimeAdapter>>,
    breaker_store: &dyn BreakerStore,
) -> ReconcileOutput {
    let plan = plan_from_policy(policy);
    let probe = RealMountProbe;
    reconciler::reconcile(ReconcileInput {
        adapters,
        probe: &probe,
        dispatcher: None,
        breaker_store,
        dry_run: plan.dry_run,
    })
    .await
}

/// The sync half of a pass: when `policy` notifies and the reconcile produced
/// actionable rows, raise the dismissable notification. Returns whether one was
/// raised. Pure db work — no awaits — so it runs after the reconcile future.
pub fn notify_pass(conn: &db::Conn, policy: RemediationPolicy, out: &ReconcileOutput) -> bool {
    let plan = plan_from_policy(policy);
    if !plan.notify {
        return false;
    }
    let summary = actionable_summary(out);
    if summary.is_empty() {
        return false;
    }
    raise_notification(conn, plan.dry_run, &summary);
    true
}

/// One full reconcile pass — the async reconcile followed by the sync notify.
/// Not `Send` (holds `conn` across the split), so it is used by tests and any
/// synchronous caller; the periodic [`tick`] instead sequences
/// [`reconcile_pass`] then [`notify_pass`] to keep its spawned future `Send`.
pub async fn run_pass(
    policy: RemediationPolicy,
    adapters: Vec<Arc<dyn containers::RuntimeAdapter>>,
    breaker_store: &dyn BreakerStore,
    conn: &db::Conn,
) -> anyhow::Result<PassOutcome> {
    let reconcile = reconcile_pass(policy, adapters, breaker_store).await;
    let notified = notify_pass(conn, policy, &reconcile);
    Ok(PassOutcome {
        reconcile,
        notified,
    })
}

/// Raise the dismissable proposal/record notification. Under a read-only pass
/// the summary is the PROPOSED action set (operator approves by opting the host
/// into an acting policy); under an acting pass it records what was applied.
/// Best-effort: a DB/notify error must never fail the tick.
fn raise_notification(conn: &db::Conn, dry_run: bool, summary: &[String]) {
    let verb = if dry_run { "proposed" } else { "applied" };
    let title = format!("Container reconcile: {} action(s) {verb}", summary.len());
    let input = RaiseInput {
        key: NOTIFY_KEY.to_string(),
        source: NOTIFY_SOURCE.to_string(),
        source_ref: None,
        severity: Severity::Warn,
        actionable: dry_run,
        fix: None,
        title,
        body: Some(summary.join("\n")),
        user_id: None,
    };
    if let Err(e) = db::notifications_store::raise(conn, input, utils::time::now().unix_millis()) {
        warn!("[containers.reconcile] notify raise failed: {e}");
    }
}

/// One reconcile pass for this host: resolve the registered adapters, read the
/// remediation policy, run the reconciler under the policy gate, and raise a
/// notification when the policy notifies. Best-effort — a db/policy error logs
/// and the loop keeps ticking (defaulting to the conservative `notify`).
async fn tick() -> anyhow::Result<()> {
    let adapters = containers::registered_adapters();
    if adapters.is_empty() {
        debug!("[containers.reconcile] no runtime adapters registered; skipping tick");
        return Ok(());
    }
    // Read the policy synchronously (conn is `!Send`), then drop the conn
    // before the reconcile await; a fresh conn is opened after for the notify.
    let policy = {
        let conn = db::open_default()?;
        remediation::policy(&conn).unwrap_or_else(|e| {
            warn!("[containers.reconcile] could not read remediation policy ({e}); defaulting to notify");
            RemediationPolicy::default()
        })
    };
    let breaker_store = reconciler::default_breaker_store();
    let out = reconcile_pass(policy, adapters, breaker_store.as_ref()).await;

    let notified = match db::open_default() {
        Ok(conn) => notify_pass(&conn, policy, &out),
        Err(e) => {
            warn!("[containers.reconcile] could not open db to notify: {e}");
            false
        }
    };

    let plan = plan_from_policy(policy);
    let n = actionable_summary(&out).len();
    if n > 0 {
        info!(
            "[containers.reconcile] policy={} dry_run={} actionable={} notified={}",
            policy.as_str(),
            plan.dry_run,
            n,
            notified
        );
    }
    for e in &out.adapter_errors {
        warn!(
            "[containers.reconcile] adapter {} list error: {}",
            e.runtime.as_str(),
            e.message
        );
    }
    for e in &out.start_errors {
        warn!(
            "[containers.reconcile] start error for {} on {}: {}",
            e.name, e.host, e.message
        );
    }
    Ok(())
}

/// Spawn the per-host container-reconcile loop. Returns the periodic handle the
/// daemon leaks for the process lifetime (scheduler convention).
pub fn spawn() -> JoinHandle<()> {
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "containers.reconcile.run",
            initial_delay: Duration::from_secs(15),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(|| async { tick().await }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use containers::{
        AdapterError, Container, ContainerState, ListFilter, LogTail, RestartPolicy,
        RuntimeAdapter, RuntimeKind,
    };
    use derive::orca_async;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Adapter over one down (`Created`, `unless-stopped`) container that counts
    /// `start()` calls so the policy gate is observable without a live runtime.
    struct MockAdapter {
        starts: Arc<AtomicUsize>,
    }

    #[orca_async]
    impl RuntimeAdapter for MockAdapter {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Docker
        }
        async fn list(&self, _filter: &ListFilter) -> Result<Vec<Container>, AdapterError> {
            Ok(vec![down_container()])
        }
        async fn inspect(&self, id: &str) -> Result<Container, AdapterError> {
            Err(AdapterError::NotFound(id.into()))
        }
        async fn start(&self, _id: &str) -> Result<(), AdapterError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
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
    }

    fn down_container() -> Container {
        Container {
            id: "abc123".into(),
            name: "sonarr".into(),
            runtime: RuntimeKind::Docker,
            host: "charlie".into(),
            state: ContainerState::Created,
            health: contract::health::Health::Unknown,
            restart_policy: RestartPolicy::UnlessStopped,
            image: None,
            labels: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            started_at: None,
            finished_at: None,
            restart_count: 0,
            exit_code: None,
            startup: None,
        }
    }

    fn adapters(starts: &Arc<AtomicUsize>) -> Vec<Arc<dyn RuntimeAdapter>> {
        vec![Arc::new(MockAdapter {
            starts: starts.clone(),
        })]
    }

    // (d) The default policy resolves to Notify and gates OUT the restart action.
    #[test]
    fn default_policy_gates_out_restart() {
        assert_eq!(RemediationPolicy::default(), RemediationPolicy::Notify);
        let plan = plan_from_policy(RemediationPolicy::default());
        assert!(
            plan.dry_run,
            "default policy must run read-only (no restart)"
        );
        assert!(plan.notify, "default policy still notifies the proposal");
    }

    // (a) A tick invokes reconcile; under an acting policy the down container is
    // restarted (start() called exactly once).
    #[tokio::test]
    async fn auto_fix_invokes_reconcile_and_restarts() {
        let starts = Arc::new(AtomicUsize::new(0));
        let store = containers::breaker::MemoryStore::new();
        let conn = db::testing::test_conn();
        let out = run_pass(RemediationPolicy::AutoFix, adapters(&starts), &store, &conn)
            .await
            .unwrap();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "auto_fix must restart the down container"
        );
        assert!(!out.notified, "auto_fix acts silently");
        assert_eq!(actionable_summary(&out.reconcile).len(), 1);
    }

    // (b) policy=Notify → NO restart action taken, notification emitted.
    #[tokio::test]
    async fn notify_does_not_restart_but_notifies() {
        let starts = Arc::new(AtomicUsize::new(0));
        let store = containers::breaker::MemoryStore::new();
        let conn = db::testing::test_conn();
        let out = run_pass(RemediationPolicy::Notify, adapters(&starts), &store, &conn)
            .await
            .unwrap();
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "notify must NEVER restart a container"
        );
        assert!(out.notified, "notify must raise the proposal notification");
        assert_eq!(actionable_summary(&out.reconcile).len(), 1);
    }

    // (c) policy=AutoFixNotify → restart action taken AND notification emitted.
    #[tokio::test]
    async fn auto_fix_notify_restarts_and_notifies() {
        let starts = Arc::new(AtomicUsize::new(0));
        let store = containers::breaker::MemoryStore::new();
        let conn = db::testing::test_conn();
        let out = run_pass(
            RemediationPolicy::AutoFixNotify,
            adapters(&starts),
            &store,
            &conn,
        )
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(out.notified);
    }

    // Disabled: read-only AND silent (no restart, no notification).
    #[tokio::test]
    async fn disabled_is_silent_and_does_not_restart() {
        let starts = Arc::new(AtomicUsize::new(0));
        let store = containers::breaker::MemoryStore::new();
        let conn = db::testing::test_conn();
        let out = run_pass(
            RemediationPolicy::Disabled,
            adapters(&starts),
            &store,
            &conn,
        )
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(!out.notified);
    }
}
