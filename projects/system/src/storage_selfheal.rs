//! Fast per-host autofs self-heal loop.
//!
//! Separate from the cron scheduler ([`crate::scheduler`]) on purpose: cron is
//! minute-resolution, and stale-mount recovery wants a tighter, seconds-scale
//! cadence. This loop ticks every [`INTERVAL_SECS`], probes every declared
//! network-share mount, and force-recovers one only after it has been stale for
//! [`CONFIRM_TICKS`] consecutive probes.
//!
//! The confirm-before-act counter is the safety valve: NFS `hard` mounts have
//! long built-in `timeo`/`retrans` patience, so a single stale probe often just
//! means the server is briefly slow. Force-unmounting on that first blip would
//! cause the very outage we're preventing. Requiring N consecutive stale probes
//! rides out transient slowness and only acts on a genuinely-down source — at
//! which point autofs remounts and fails over to the next ordered source.
//!
//! Tuned defaults give ~60–90s worst-case recovery (CONFIRM_TICKS × INTERVAL +
//! remount), near-instant for a cleanly-unreachable server. Deliberately not
//! sub-10s — that range is false-positive territory for network fs.

use crate::remediation::{self, RemediationDomain, RemediationPolicy};
use crate::remediation_controller::{Finding, Outcome, Remediator};
use crate::source_election::{RemountAggression, Transition};
use crate::{autofs, managed_mounts};
use db::notifications_store::{Fix, RaiseInput, Severity};
use derive::orca_async;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// `source` all self-heal dismissable notifications carry, for scoping.
const NOTIFY_SOURCE: &str = "remediation:storage.selfheal";

/// Seconds between self-heal ticks. Recovery latency ≈ this × [`CONFIRM_TICKS`].
pub const INTERVAL_SECS: u64 = 30;
/// Per-target liveness-probe timeout. A live NFS `stat` answers in ms; this long
/// a hang means the server is unreachable, not merely slow.
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Consecutive stale probes required before a target is force-recovered. The
/// blip filter — 2 ticks (~60s) rides out transient server slowness.
pub const CONFIRM_TICKS: u32 = 2;

/// The storage self-heal [`Remediator`] — the first consumer of the generic
/// remediation controller. Domain [`RemediationDomain::NfsMount`].
///
/// autofs storage failover interleaves detection and recovery too tightly to
/// express as a clean detect/remediate split without risking the live-fleet
/// failover mechanics: source election, the CONFIRM_TICKS blip filter, the
/// per-target reload gate, and the backend consumer sweep all read and act in
/// one pass. So this remediator keeps that proven [`tick`] intact and self-gated
/// (it reads the same per-domain policy the controller would) and surfaces no
/// [`Finding`] to the generic gate. The controller's role here is to drive the
/// single loop; decomposing this into the generic detect/remediate gate is a
/// follow-up.
pub struct StorageSelfheal {
    /// Per-target consecutive-stale counters, shared across ticks. The `Mutex`
    /// guards the map, held only for the (await-free) bookkeeping in [`tick`].
    counters: Mutex<HashMap<String, u32>>,
}

/// Build the storage self-heal remediator for registration with the controller.
pub fn remediator() -> Arc<dyn Remediator> {
    Arc::new(StorageSelfheal {
        counters: Mutex::new(HashMap::new()),
    })
}

#[orca_async]
impl Remediator for StorageSelfheal {
    fn name(&self) -> &str {
        "storage.selfheal"
    }

    fn domain(&self) -> RemediationDomain {
        RemediationDomain::NfsMount
    }

    async fn detect(&self) -> Vec<Finding> {
        // The full self-gated pass — probe, elect, reconcile, recover — runs
        // here. A tick error must not stop the controller loop; log and move on.
        if let Err(e) = tick(&self.counters).await {
            warn!("[selfheal] tick: {e:#}");
        }
        Vec::new()
    }

    async fn remediate(&self, _finding: &Finding) -> anyhow::Result<Outcome> {
        // Never reached: `detect` surfaces no findings to the generic gate.
        Ok(Outcome {
            acted: false,
            summary: String::new(),
        })
    }
}

/// One self-heal pass: probe declared network-share targets, advance the
/// consecutive-stale counters, and recover any target that has crossed
/// [`CONFIRM_TICKS`]. Counters for now-healthy or removed mounts are cleared so
/// a recovered mount must go stale afresh before acting again.
async fn tick(counters: &Mutex<HashMap<String, u32>>) -> anyhow::Result<()> {
    let mounts: Vec<managed_mounts::ManagedMount> = managed_mounts::endpoint_db::list()?
        .into_iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .collect();
    if mounts.is_empty() {
        return Ok(());
    }

    // The remediation gate. Read-only probing/election-*evaluation* below runs
    // unconditionally (it is how we detect); only the state-changing recovery is
    // governed by this policy. Defaults to `notify` when unset — never auto-act
    // until the operator opts in.
    let policy = current_policy();

    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Election pass — the autofs-can't-do-it half: elect the first live source
    // per mount, re-render the map to that single source when it changed, and
    // remount (safely) if the kernel is mounted from the wrong one. Runs every
    // tick regardless of staleness: a healthy mount can still be on the *wrong*
    // source (secondary while primary is live again), which the stale pass below
    // would never catch. Non-silent on every degrade / failback / empty-target.
    // The election *evaluation* is always performed; whether it *acts* on the
    // result is governed by the policy.
    elect_and_reconcile(&mounts, policy).await;

    let targets: Vec<String> = mounts.iter().map(|m| m.target.clone()).collect();
    let stale: std::collections::HashSet<String> = autofs::probe_stale(&targets, timeout)
        .await
        .into_iter()
        .collect();

    // Update counters and decide who to act on, holding the lock only for the
    // bookkeeping (no `.await` while locked).
    let to_recover = {
        let mut counts = counters.lock().expect("selfheal counters poisoned");
        advance_counters(&mut counts, &targets, &stale)
    };

    for target in &to_recover {
        // Remediation gate on the stale-mount force-recover. `to_recover` has
        // already survived CONFIRM_TICKS, so under `notify` we raise at most one
        // dismissable notification per confirm window (natural debounce), and
        // under `disabled` we take no action at all.
        if !policy.acts() {
            if policy.notifies() {
                raise_notification(
                    format!("remediation:selfheal:recover:{target}"),
                    Severity::Warn,
                    true,
                    format!("Stale mount {target} needs recovery"),
                    format!(
                        "{target} has been stale for {CONFIRM_TICKS} consecutive probes. \
                         Proposed remediation: force-recover (umount -lf + retrigger, failing \
                         over to the next live source). Approve to let orca act, or set the \
                         host remediation policy to auto_fix."
                    ),
                    Some(Fix {
                        unit: Some(target.clone()),
                        action: Some("recover".into()),
                        ..Default::default()
                    }),
                );
            } else {
                debug!(
                    "[selfheal] remediation disabled; not force-recovering stale mount {target}"
                );
            }
            continue;
        }
        // A forced autofs reload is global, so only allow it when this share's
        // server is actually reachable (server up, autofs simply isn't serving the
        // map — the freyr case). If the source is down, a reload can't help and
        // would churn every *other* healthy mount; recover with unmount+retrigger
        // only. Gated per-target on live-source election.
        let allow_reload = match mounts.iter().find(|m| &m.target == target) {
            Some(m) => matches!(
                autofs::elect_live_source(m, timeout).await,
                crate::source_election::Election::Elected { .. }
            ),
            None => false,
        };
        let (recovered, errors) = autofs::force_and_retrigger(target, allow_reload, timeout).await;
        if recovered {
            info!("[selfheal] recovered stale mount {target} (failed over)");
            if policy.notifies() {
                raise_notification(
                    format!("remediation:selfheal:recovered:{target}"),
                    Severity::Info,
                    false,
                    format!("Recovered stale mount {target}"),
                    format!("{target} was stale and has been force-recovered (failed over)."),
                    None,
                );
            }
        } else if allow_reload {
            // Source is reachable but we still couldn't make the mount live after a
            // reload — a genuine, actionable fault, not a transient down-server.
            warn!(
                "[selfheal] {target} source is reachable but mount is still not live \
                 after reload+retrigger; errors={errors:?}"
            );
        } else {
            warn!("[selfheal] {target} still stale (source down); errors={errors:?}");
        }
    }

    // Backend-routed consumer sweep — runs every tick (not debounced) when the
    // policy permits acting. Core's probe+debounce above only sees *host-mount*
    // staleness; it can never catch the case where the host mount is healthy but
    // a container pins a stale NFS superblock (ESTALE inside the guest). That is
    // exactly what a recover-capable backend (nfs's `recover_stale` →
    // consumer-aware bind-mount heal) detects and repairs. The plugin gates its
    // own consumer restarts behind a host-healthy + consumer-stale guard, so
    // calling it each tick cannot storm; core adds no second restart path and
    // never restarts containers itself. This sweep both detects AND repairs, so
    // under a non-acting policy it is skipped entirely (there is no read-only
    // probe to run without the repair).
    if policy.acts() {
        let merged = crate::storage_tools::recover_via_backends(&mounts, timeout).await;
        for t in &merged.recovered {
            info!("[selfheal] backend recovered {t}");
        }
        for t in &merged.remounted {
            info!("[selfheal] backend remounted absent mount {t}");
        }
        for t in &merged.still_stale {
            warn!("[selfheal] backend reports {t} still stale after recovery");
        }
        for t in &merged.still_missing {
            warn!("[selfheal] backend could not remount absent mount {t}");
        }
        for e in &merged.errors {
            warn!("[selfheal] backend recover error: {e}");
        }
        if policy.notifies() && !merged.recovered.is_empty() {
            raise_notification(
                "remediation:selfheal:backend-recovered".to_string(),
                Severity::Info,
                false,
                "Backend recovered stale consumer mounts".to_string(),
                format!("Recovered consumer-stale mounts: {:?}", merged.recovered),
                None,
            );
        }
    } else {
        debug!("[selfheal] remediation disabled; skipping backend consumer recovery sweep");
    }
    Ok(())
}

/// The election + failback pass. For every managed network share: elect its
/// first live source, re-render `/etc/auto.orca` with the single elected source
/// per mount (idempotent — a no-op when nothing changed), then reconcile the
/// actual kernel mount to the elected source per each mount's remount policy
/// (default [`RemountAggression::Safe`] — never disrupt a busy mount). Every
/// transition is logged non-silently.
///
/// The election *evaluation* (electing the live source, logging the result) is
/// read-only and runs regardless of `policy`. The state-changing half — the map
/// re-render, the remount reconcile, and the proactive trigger — only runs when
/// `policy.acts()`. Under a non-acting policy the detected degrade/empty
/// conditions are surfaced as dismissable notifications (`notify`) or logged at
/// debug and dropped (`disabled`).
async fn elect_and_reconcile(mounts: &[managed_mounts::ManagedMount], policy: RemediationPolicy) {
    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Elect once per mount so the map and the remount decision agree. Capture
    // the degrade/empty conditions so a non-acting policy can propose/report them.
    let mut elected: HashMap<String, String> = HashMap::new();
    let mut degraded: Vec<(String, String, usize)> = Vec::new();
    let mut empty_targets: Vec<String> = Vec::new();
    for m in mounts {
        match autofs::elect_live_source(m, timeout).await {
            crate::source_election::Election::Elected { source, index } => {
                if index > 0 {
                    info!(
                        "[election] {} elected failover source #{index} {source} \
                         (higher-priority source down)",
                        m.target
                    );
                    degraded.push((m.target.clone(), source.clone(), index));
                }
                elected.insert(m.target.clone(), source);
            }
            crate::source_election::Election::Empty => {
                warn!(
                    "[election] {} has NO live source — all {} ordered sources down; \
                     leaving map entry empty",
                    m.target,
                    managed_mounts::ordered_sources(&m.source, m.failover_sources.as_deref()).len()
                );
                empty_targets.push(m.target.clone());
            }
        }
    }

    // Remediation gate on the state-changing half. Under a non-acting policy we
    // never re-render the map, remount, or trigger — we only surface the
    // detected conditions per policy and return.
    if !policy.acts() {
        if policy.notifies() {
            for (target, source, index) in &degraded {
                raise_notification(
                    format!("remediation:selfheal:election:{target}"),
                    Severity::Warn,
                    true,
                    format!("{target}: higher-priority storage source down"),
                    format!(
                        "Proposed remediation: fail {target} over to source #{index} {source} \
                         (re-render autofs map + remount). Approve or set remediation policy to \
                         auto_fix."
                    ),
                    Some(Fix {
                        unit: Some(target.clone()),
                        action: Some("apply".into()),
                        ..Default::default()
                    }),
                );
            }
            for target in &empty_targets {
                raise_notification(
                    format!("remediation:selfheal:empty:{target}"),
                    Severity::Error,
                    false,
                    format!("{target}: all storage sources down"),
                    format!(
                        "{target} has no live source — every ordered source is unreachable. \
                         orca cannot fail over; the source servers need attention."
                    ),
                    None,
                );
            }
        } else if !degraded.is_empty() || !empty_targets.is_empty() {
            debug!(
                "[selfheal] remediation disabled; not reconciling {} degraded / {} empty mount(s)",
                degraded.len(),
                empty_targets.len()
            );
        }
        return;
    }

    // Re-render the elected single-source map and apply (privileged, idempotent:
    // no privileged call when the on-disk map already matches).
    let outcome = autofs::apply_elected(mounts, &elected).await;
    if !outcome.changed.is_empty() {
        info!(
            "[election] re-rendered autofs map with elected sources: changed={:?} reloaded={}",
            outcome.changed, outcome.reloaded
        );
    }
    for e in &outcome.errors {
        warn!("[election] map apply error: {e}");
    }

    // Reconcile each mount's live source to the election, logging the transition.
    for m in mounts {
        let aggression = RemountAggression::from_policy(m.remount_policy.as_deref());
        let (trans, errors) = autofs::reconcile_source(m, aggression, timeout).await;
        match &trans {
            Transition::Unchanged | Transition::EmptyTarget => {}
            Transition::FailBack { to } => info!(
                "[election] {} failing back to primary-preferred source {to} \
                 (aggression={aggression:?})",
                m.target
            ),
            Transition::Degrade { to } => {
                warn!(
                    "[election] {} degrading to source {to} (higher-priority source down; \
                     aggression={aggression:?})",
                    m.target
                );
                if policy.notifies() {
                    raise_notification(
                        format!("remediation:selfheal:election:{}", m.target),
                        Severity::Info,
                        false,
                        format!("{}: failed over to {to}", m.target),
                        format!(
                            "orca degraded {} to source {to} (higher-priority source down).",
                            m.target
                        ),
                        None,
                    );
                }
            }
            Transition::Mount { to } => info!(
                "[election] {} mounting elected source {to} (aggression={aggression:?})",
                m.target
            ),
        }
        for err in errors {
            warn!("[election] {} remount error: {err}", m.target);
        }
    }

    // Proactively bring up every managed target this tick (mount-before-bind).
    // A direct-map mountpoint mounts on access; with persistent (timeout=0)
    // mounts this trigger keeps declared paths mounted so a container bind of a
    // subpath never races an unmounted path (which would let Docker create a
    // local shadow dir that blocks autofs). `trigger` is idempotent — an
    // already-mounted path stays mounted, so this causes no churn. Best-effort:
    // log and continue; a trigger error must never fail the self-heal tick.
    let targets: Vec<String> = mounts
        .iter()
        .filter(|m| m.enabled && m.kind == "network_share")
        .map(|m| m.target.clone())
        .collect();
    for err in autofs::trigger(&targets).await {
        warn!("[selfheal] mount trigger error: {err}");
    }
}

/// Read the local host's remediation policy for this tick. On a DB error the
/// loop must keep running, so fall back to the conservative default
/// ([`RemediationPolicy::Notify`] — never auto-act).
fn current_policy() -> RemediationPolicy {
    match db::open_default() {
        Ok(conn) => remediation::policy(&conn, RemediationDomain::NfsMount).unwrap_or_default(),
        Err(e) => {
            warn!("[selfheal] could not read remediation policy ({e}); defaulting to notify");
            RemediationPolicy::default()
        }
    }
}

/// Raise a dismissable notification carrying a proposed (or completed)
/// remediation. Re-raising the same `key` is an idempotent upsert, so a
/// still-unresolved condition surfaces as a single row rather than spamming.
/// Best-effort: a DB/notify error must never fail the self-heal tick.
fn raise_notification(
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
            warn!("[selfheal] notify: open db: {e}");
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
    if let Err(e) = db::notifications_store::raise(&conn, input, now_ms()) {
        warn!("[selfheal] notify: raise: {e}");
    }
}

fn now_ms() -> i64 {
    utils::time::now().unix_millis()
}

/// Advance the per-target consecutive-stale counters for one probe pass and
/// return the targets that have crossed [`CONFIRM_TICKS`] and should be
/// recovered. Pure bookkeeping (no I/O): counters for removed mounts are
/// dropped, a healthy target clears its streak, and a target that fires is
/// reset to 0 so a still-down mount must re-confirm before re-acting.
fn advance_counters(
    counts: &mut HashMap<String, u32>,
    targets: &[String],
    stale: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut to_recover = Vec::new();
    counts.retain(|target, _| targets.contains(target)); // drop removed mounts
    for target in targets {
        if stale.contains(target) {
            let c = counts.entry(target.clone()).or_insert(0);
            *c += 1;
            if *c >= CONFIRM_TICKS {
                to_recover.push(target.clone());
                *c = 0; // reset so a still-down mount re-confirms before re-acting
            }
        } else {
            counts.remove(target); // healthy → clear any streak
        }
    }
    to_recover
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn vec(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn constants_are_sane() {
        assert_eq!(INTERVAL_SECS, 30);
        assert_eq!(PROBE_TIMEOUT_SECS, 5);
        assert_eq!(CONFIRM_TICKS, 2);
    }

    #[test]
    fn single_stale_probe_does_not_recover() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a"]);
        let out = advance_counters(&mut counts, &targets, &set(&["/mnt/a"]));
        assert!(out.is_empty());
        assert_eq!(counts["/mnt/a"], 1);
    }

    #[test]
    fn confirm_ticks_stale_probes_recover_and_reset() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a"]);
        // tick 1: streak 1, no recover
        assert!(advance_counters(&mut counts, &targets, &set(&["/mnt/a"])).is_empty());
        // tick 2: hits CONFIRM_TICKS → recover, counter reset to 0
        let out = advance_counters(&mut counts, &targets, &set(&["/mnt/a"]));
        assert_eq!(out, vec(&["/mnt/a"]));
        assert_eq!(counts["/mnt/a"], 0);
    }

    #[test]
    fn still_down_must_reconfirm_before_reacting() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a"]);
        advance_counters(&mut counts, &targets, &set(&["/mnt/a"])); // 1
        advance_counters(&mut counts, &targets, &set(&["/mnt/a"])); // recover, reset 0
        // next tick: streak restarts at 1, no immediate re-recover
        let out = advance_counters(&mut counts, &targets, &set(&["/mnt/a"]));
        assert!(out.is_empty());
        assert_eq!(counts["/mnt/a"], 1);
    }

    #[test]
    fn healthy_probe_clears_streak() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a"]);
        advance_counters(&mut counts, &targets, &set(&["/mnt/a"])); // streak 1
        let out = advance_counters(&mut counts, &targets, &set(&[])); // healthy
        assert!(out.is_empty());
        assert!(!counts.contains_key("/mnt/a"));
    }

    #[test]
    fn intermittent_stale_never_reaches_confirm() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a"]);
        for _ in 0..5 {
            // one stale blip (streak → 1, never recovers)...
            let out = advance_counters(&mut counts, &targets, &set(&["/mnt/a"]));
            assert!(out.is_empty());
            // ...then healthy resets the streak before it can reach CONFIRM_TICKS
            advance_counters(&mut counts, &targets, &set(&[]));
        }
        assert!(!counts.contains_key("/mnt/a"));
    }

    #[test]
    fn removed_mount_counter_is_dropped() {
        let mut counts = HashMap::new();
        advance_counters(&mut counts, &vec(&["/mnt/a"]), &set(&["/mnt/a"])); // streak on a
        assert!(counts.contains_key("/mnt/a"));
        // /mnt/a no longer declared → its counter is purged
        let out = advance_counters(&mut counts, &vec(&["/mnt/b"]), &set(&["/mnt/b"]));
        assert!(out.is_empty());
        assert!(!counts.contains_key("/mnt/a"));
        assert_eq!(counts["/mnt/b"], 1);
    }

    #[test]
    fn independent_targets_track_separately() {
        let mut counts = HashMap::new();
        let targets = vec(&["/mnt/a", "/mnt/b"]);
        // a stale twice → recovers; b stale once → not yet
        advance_counters(&mut counts, &targets, &set(&["/mnt/a", "/mnt/b"]));
        let out = advance_counters(&mut counts, &targets, &set(&["/mnt/a"]));
        assert_eq!(out, vec(&["/mnt/a"]));
        // b was healthy on the second tick → cleared
        assert!(!counts.contains_key("/mnt/b"));
    }
}
