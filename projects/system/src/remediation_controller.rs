//! Generic, policy-gated remediation controller.
//!
//! One background loop drives every registered [`Remediator`]. Each tick, for
//! each remediator, the loop:
//!
//! 1. runs the read-only [`Remediator::detect`] — **always**, regardless of
//!    policy: detection is how orca sees a fault, and observing is never gated;
//! 2. resolves the per-domain [`RemediationPolicy`](crate::remediation::RemediationPolicy)
//!    via [`remediation::policy`] for that remediator's
//!    [`domain`](Remediator::domain);
//! 3. acts on each [`Finding`] **only per that policy**:
//!    * `auto_fix`        — [`remediate`](Remediator::remediate) silently;
//!    * `auto_fix_notify` — remediate AND raise a notification of what was done;
//!    * `notify`          — do NOT act; raise a dismissable notification carrying
//!      the proposed remediation (debounced to once per unresolved finding key);
//!    * `disabled`        — nothing (detection still ran).
//!
//! This is the seam that lets any subsystem plug into a single self-heal loop
//! with a uniform operator gate. [`crate::storage_selfheal`] is the first
//! consumer; container reconcile/wedge/breaker and plugin-owned remediators
//! (proxmox guest, docker-engine) follow.
//!
//! The rusqlite connection is not `Send`, so it must never be held across an
//! `.await`. Policies are therefore resolved up front (connection scoped and
//! dropped before any detect/remediate future runs), and notifications are
//! raised through a [`NotificationSink`] whose production impl opens its own
//! short-lived connection synchronously.

use crate::periodic;
use crate::remediation::{self, RemediationDomain, RemediationPolicy};
use contract::health::Health;
use db::notifications_store::{Fix, RaiseInput, Severity};
use derive::orca_async;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// `source` every controller-raised dismissable notification carries.
const NOTIFY_SOURCE: &str = "remediation:controller";

/// Seconds between controller ticks. Mirrors the storage self-heal cadence so
/// the first consumer keeps its ~seconds-scale recovery latency.
pub const INTERVAL_SECS: u64 = 30;
/// Delay before the first tick — don't slam the daemon at startup.
pub const INITIAL_DELAY_SECS: u64 = 10;

/// A detected condition a [`Remediator`] can act on. `key` is the stable
/// identity of the *condition* (not the run): it debounces notifications and
/// keys the notification row, so a still-unresolved fault surfaces once rather
/// than every tick.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Stable, unresolved-condition-scoped key (e.g. `nfs_mount:/mnt/media`).
    pub key: String,
    /// Health of the thing this finding concerns.
    pub health: Health,
    /// Severity to stamp on any notification raised for this finding.
    pub severity: Severity,
    /// One-line human summary — the notification title.
    pub summary: String,
    /// Longer human description — the notification body.
    pub detail: String,
    /// The proposed remediation, carried on the notification under a non-acting
    /// policy so the operator can approve it.
    pub fix: Option<Fix>,
}

/// The result of a [`Remediator::remediate`] call.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Whether a state-changing action was actually taken (vs. a no-op because
    /// the condition had already cleared by the time remediation ran).
    pub acted: bool,
    /// Human summary of what was done — the body of the `auto_fix_notify` record.
    pub summary: String,
}

/// A pluggable self-heal unit. `detect` is read-only and always runs; only
/// `remediate` is governed by the host's per-domain remediation policy.
#[orca_async]
pub trait Remediator: Send + Sync {
    /// Stable name for logs and notification bodies.
    fn name(&self) -> &str;

    /// Which policy domain governs this remediator's state-changing actions.
    fn domain(&self) -> RemediationDomain;

    /// Read-only detection. Returns every currently-actionable [`Finding`].
    /// Runs every tick regardless of policy.
    async fn detect(&self) -> Vec<Finding>;

    /// Take the state-changing action for one `finding`. Called only when the
    /// resolved policy permits acting.
    async fn remediate(&self, finding: &Finding) -> anyhow::Result<Outcome>;
}

static REGISTRY: LazyLock<RwLock<Vec<Arc<dyn Remediator>>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

/// Register a remediator with the process-global controller. Host bootstrap
/// calls this once per remediator before [`spawn`].
pub fn register(remediator: Arc<dyn Remediator>) {
    REGISTRY
        .write()
        .expect("remediator registry poisoned")
        .push(remediator);
}

/// Snapshot of the registered remediators (cheap `Arc` clones).
pub fn registered() -> Vec<Arc<dyn Remediator>> {
    REGISTRY
        .read()
        .expect("remediator registry poisoned")
        .iter()
        .cloned()
        .collect()
}

/// A sink for controller-raised notifications. The production
/// [`DbSink`] opens a short-lived connection per raise; tests substitute a
/// no-op so the loop's policy branching can be asserted without a database.
pub trait NotificationSink: Send + Sync {
    /// Raise (idempotent-upsert) one dismissable notification.
    fn raise(
        &self,
        key: String,
        severity: Severity,
        actionable: bool,
        title: String,
        body: String,
        fix: Option<Fix>,
    );
}

/// Production sink: writes to the local notifications store. Best-effort — a DB
/// error must never fail the controller tick.
pub struct DbSink;

impl NotificationSink for DbSink {
    fn raise(
        &self,
        key: String,
        severity: Severity,
        actionable: bool,
        title: String,
        body: String,
        fix: Option<Fix>,
    ) {
        let conn = match db::open_default() {
            Ok(c) => c,
            Err(e) => {
                warn!("[remediation] notify: open db: {e}");
                return;
            }
        };
        let input = RaiseInput {
            key,
            source: NOTIFY_SOURCE.to_string(),
            source_ref: None,
            severity,
            actionable,
            fix,
            title,
            body: Some(body),
            user_id: None,
        };
        if let Err(e) =
            db::notifications_store::raise(&conn, input, utils::time::now().unix_millis())
        {
            warn!("[remediation] notify: raise: {e}");
        }
    }
}

/// Spawn the single controller loop. Returns the periodic-loop handle; the
/// daemon drops it for the process lifetime, matching the scheduler convention.
pub fn spawn() -> JoinHandle<()> {
    // Keys already notified for an unresolved finding, so `notify` policy raises
    // once per condition rather than every tick. Pruned when a key stops
    // appearing (the condition resolved).
    let notified = Arc::new(Mutex::new(HashSet::<String>::new()));
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "remediation.controller.run",
            initial_delay: Duration::from_secs(INITIAL_DELAY_SECS),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(move || {
            let notified = notified.clone();
            async move { tick(&notified).await }
        }),
    )
}

/// One controller pass over every registered remediator. Resolves each
/// remediator's policy up front (connection scoped, never held across an await),
/// then delegates to [`run_pass`] with the production notification sink.
async fn tick(notified: &Mutex<HashSet<String>>) -> anyhow::Result<()> {
    let remediators = registered();
    let policies = resolve_policies(&remediators);
    run_pass(&remediators, &policies, notified, &DbSink).await;
    Ok(())
}

/// Resolve each remediator's per-domain policy in one connection scope. The
/// connection is dropped before returning, so no non-`Send` handle survives into
/// the async pass. A DB error falls back to the domain default (never more
/// autonomous than the operator opted into).
fn resolve_policies(remediators: &[Arc<dyn Remediator>]) -> Vec<RemediationPolicy> {
    let conn = db::open_default().ok();
    remediators
        .iter()
        .map(|r| {
            let domain = r.domain();
            match conn.as_ref().map(|c| remediation::policy(c, domain)) {
                Some(Ok(p)) => p,
                Some(Err(e)) => {
                    warn!(
                        "[remediation] {}: policy read failed ({e}); using {} default",
                        r.name(),
                        domain.as_str()
                    );
                    domain.default_policy()
                }
                None => domain.default_policy(),
            }
        })
        .collect()
}

/// What a single pass decided, for observability and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Finding keys a remediator acted on this pass.
    pub remediated: Vec<String>,
    /// Finding keys that raised a notification this pass — the debounced
    /// proposal (`notify`) or the record of an action taken (`auto_fix_notify`).
    pub notified: Vec<String>,
}

/// Run one pass: detect (always) → act/notify per each remediator's resolved
/// policy. Debounces the proposal notification to once per unresolved key.
///
/// `policies[i]` governs `remediators[i]`. Split from [`tick`] so tests can
/// drive it with a fake remediator, explicit policies, and a no-op sink — no
/// database required.
async fn run_pass(
    remediators: &[Arc<dyn Remediator>],
    policies: &[RemediationPolicy],
    notified: &Mutex<HashSet<String>>,
    sink: &dyn NotificationSink,
) -> PassReport {
    let mut report = PassReport::default();
    let mut live_keys = HashSet::<String>::new();

    for (r, &policy) in remediators.iter().zip(policies) {
        // Read-only detection ALWAYS runs — observing a fault is never gated.
        let findings = r.detect().await;

        for f in &findings {
            live_keys.insert(f.key.clone());

            if policy.acts() {
                match r.remediate(f).await {
                    Ok(outcome) if outcome.acted => {
                        info!("[remediation] {}: {}", r.name(), outcome.summary);
                        report.remediated.push(f.key.clone());
                        if policy.notifies() {
                            sink.raise(
                                f.key.clone(),
                                Severity::Info,
                                false,
                                f.summary.clone(),
                                outcome.summary,
                                None,
                            );
                            report.notified.push(f.key.clone());
                        }
                    }
                    Ok(_) => debug!("[remediation] {}: {} already clear", r.name(), f.key),
                    Err(e) => warn!("[remediation] {}: remediate {}: {e:#}", r.name(), f.key),
                }
            } else if policy.notifies() {
                // Debounce: raise the proposal once per unresolved finding key.
                let first = notified
                    .lock()
                    .expect("remediation notify set poisoned")
                    .insert(f.key.clone());
                if first {
                    sink.raise(
                        f.key.clone(),
                        f.severity,
                        true,
                        f.summary.clone(),
                        f.detail.clone(),
                        f.fix.clone(),
                    );
                    report.notified.push(f.key.clone());
                }
            } else {
                debug!(
                    "[remediation] {}: policy disabled; not acting on {}",
                    r.name(),
                    f.key
                );
            }
        }
    }

    // Prune debounce entries whose conditions no longer surface — a recurrence
    // must re-notify.
    notified
        .lock()
        .expect("remediation notify set poisoned")
        .retain(|k| live_keys.contains(k));

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A no-op sink — tests assert on the returned [`PassReport`], which already
    /// records every raise decision, so the sink itself need not capture.
    struct NoopSink;
    impl NotificationSink for NoopSink {
        fn raise(&self, _: String, _: Severity, _: bool, _: String, _: String, _: Option<Fix>) {}
    }

    /// A scripted remediator: always detects one fixed finding, counts `detect`
    /// calls, and records every `remediate` call's finding key.
    struct FakeRemediator {
        detects: Arc<AtomicUsize>,
        remediated: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRemediator {
        fn new() -> (Arc<Self>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
            let detects = Arc::new(AtomicUsize::new(0));
            let remediated = Arc::new(Mutex::new(Vec::new()));
            let me = Arc::new(Self {
                detects: detects.clone(),
                remediated: remediated.clone(),
            });
            (me, detects, remediated)
        }
    }

    #[orca_async]
    impl Remediator for FakeRemediator {
        fn name(&self) -> &str {
            "test.fake"
        }
        fn domain(&self) -> RemediationDomain {
            RemediationDomain::Service
        }
        async fn detect(&self) -> Vec<Finding> {
            self.detects.fetch_add(1, Ordering::SeqCst);
            vec![Finding {
                key: "test:cond".to_string(),
                health: Health::Unhealthy,
                severity: Severity::Warn,
                summary: "test condition".to_string(),
                detail: "proposed: fix the test condition".to_string(),
                fix: Some(Fix {
                    unit: Some("test".to_string()),
                    action: Some("fix".to_string()),
                    ..Default::default()
                }),
            }]
        }
        async fn remediate(&self, finding: &Finding) -> anyhow::Result<Outcome> {
            self.remediated.lock().unwrap().push(finding.key.clone());
            Ok(Outcome {
                acted: true,
                summary: format!("fixed {}", finding.key),
            })
        }
    }

    async fn one_pass(policy: RemediationPolicy) -> (PassReport, usize, Vec<String>) {
        let (fake, detects, remediated) = FakeRemediator::new();
        let r: Arc<dyn Remediator> = fake;
        let notified = Mutex::new(HashSet::new());
        let report = run_pass(
            std::slice::from_ref(&r),
            std::slice::from_ref(&policy),
            &notified,
            &NoopSink,
        )
        .await;
        let calls = remediated.lock().unwrap().clone();
        (report, detects.load(Ordering::SeqCst), calls)
    }

    #[tokio::test]
    async fn auto_fix_acts_without_notifying() {
        let (report, detects, remediated) = one_pass(RemediationPolicy::AutoFix).await;
        assert_eq!(detects, 1, "detect must run");
        assert_eq!(remediated.as_slice(), ["test:cond"]);
        assert_eq!(report.remediated.as_slice(), ["test:cond"]);
        assert!(report.notified.is_empty(), "auto_fix is silent");
    }

    #[tokio::test]
    async fn auto_fix_notify_acts_and_notifies() {
        let (report, detects, remediated) = one_pass(RemediationPolicy::AutoFixNotify).await;
        assert_eq!(detects, 1, "detect must run");
        assert_eq!(remediated.as_slice(), ["test:cond"]);
        assert_eq!(report.remediated.as_slice(), ["test:cond"]);
        assert_eq!(report.notified.as_slice(), ["test:cond"], "reports action");
    }

    #[tokio::test]
    async fn notify_notifies_without_acting() {
        let (report, detects, remediated) = one_pass(RemediationPolicy::Notify).await;
        assert_eq!(detects, 1, "detect must run");
        assert!(remediated.is_empty(), "notify never acts");
        assert!(report.remediated.is_empty());
        assert_eq!(report.notified.as_slice(), ["test:cond"]);
    }

    #[tokio::test]
    async fn disabled_does_nothing_but_detect_still_runs() {
        let (report, detects, remediated) = one_pass(RemediationPolicy::Disabled).await;
        assert_eq!(detects, 1, "detect must run");
        assert!(remediated.is_empty());
        assert!(report.remediated.is_empty());
        assert!(report.notified.is_empty());
    }

    #[tokio::test]
    async fn notify_debounces_across_passes() {
        let (fake, _detects, _remediated) = FakeRemediator::new();
        let r: Arc<dyn Remediator> = fake;
        let remediators = std::slice::from_ref(&r);
        let policies = [RemediationPolicy::Notify];
        let notified = Mutex::new(HashSet::new());

        let first = run_pass(remediators, &policies, &notified, &NoopSink).await;
        assert_eq!(first.notified.as_slice(), ["test:cond"], "raises once");
        let second = run_pass(remediators, &policies, &notified, &NoopSink).await;
        assert!(second.notified.is_empty(), "same key is debounced");
    }
}
