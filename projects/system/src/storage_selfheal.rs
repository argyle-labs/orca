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

use crate::source_election::{RemountAggression, Transition};
use crate::{autofs, managed_mounts, periodic};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Seconds between self-heal ticks. Recovery latency ≈ this × [`CONFIRM_TICKS`].
pub const INTERVAL_SECS: u64 = 30;
/// Per-target liveness-probe timeout. A live NFS `stat` answers in ms; this long
/// a hang means the server is unreachable, not merely slow.
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Consecutive stale probes required before a target is force-recovered. The
/// blip filter — 2 ticks (~60s) rides out transient server slowness.
pub const CONFIRM_TICKS: u32 = 2;

/// Spawn the self-heal loop. Returns the periodic-loop handle; the daemon drops
/// it ("leaks it") for the process lifetime, matching the scheduler convention.
pub fn spawn() -> JoinHandle<()> {
    // Per-target consecutive-stale counters, shared across ticks. `Arc` so each
    // tick's future can own a clone (the future must be `'static`); the `Mutex`
    // guards the map, held only for the (await-free) bookkeeping in `tick`.
    let counters = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "storage.selfheal.run",
            initial_delay: Duration::from_secs(10),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(move || {
            let counters = counters.clone();
            async move { tick(&counters).await }
        }),
    )
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

    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Election pass — the autofs-can't-do-it half: elect the first live source
    // per mount, re-render the map to that single source when it changed, and
    // remount (safely) if the kernel is mounted from the wrong one. Runs every
    // tick regardless of staleness: a healthy mount can still be on the *wrong*
    // source (secondary while primary is live again), which the stale pass below
    // would never catch. Non-silent on every degrade / failback / empty-target.
    elect_and_reconcile(&mounts).await;

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
        let (recovered, errors) = autofs::force_and_retrigger(target, timeout).await;
        if recovered {
            info!("[selfheal] recovered stale mount {target} (failed over)");
        } else {
            warn!("[selfheal] {target} still stale after recovery; errors={errors:?}");
        }
    }

    // Backend-routed consumer sweep — runs every tick (not debounced). Core's
    // probe+debounce above only sees *host-mount* staleness; it can never catch
    // the case where the host mount is healthy but a container pins a stale NFS
    // superblock (ESTALE inside the guest). That is exactly what a recover-capable
    // backend (nfs's `recover_stale` → consumer-aware bind-mount heal) detects and
    // repairs. The plugin gates its own consumer restarts behind a
    // host-healthy + consumer-stale guard, so calling it each tick cannot storm;
    // core adds no second restart path and never restarts containers itself.
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
    Ok(())
}

/// The election + failback pass. For every managed network share: elect its
/// first live source, re-render `/etc/auto.orca` with the single elected source
/// per mount (idempotent — a no-op when nothing changed), then reconcile the
/// actual kernel mount to the elected source per each mount's remount policy
/// (default [`RemountAggression::Safe`] — never disrupt a busy mount). Every
/// transition is logged non-silently.
async fn elect_and_reconcile(mounts: &[managed_mounts::ManagedMount]) {
    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Elect once per mount so the map and the remount decision agree.
    let mut elected: HashMap<String, String> = HashMap::new();
    for m in mounts {
        match autofs::elect_live_source(m, timeout).await {
            crate::source_election::Election::Elected { source, index } => {
                if index > 0 {
                    info!(
                        "[election] {} elected failover source #{index} {source} \
                         (higher-priority source down)",
                        m.target
                    );
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
            }
        }
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
            Transition::Degrade { to } => warn!(
                "[election] {} degrading to source {to} (higher-priority source down; \
                 aggression={aggression:?})",
                m.target
            ),
            Transition::Mount { to } => info!(
                "[election] {} mounting elected source {to} (aggression={aggression:?})",
                m.target
            ),
        }
        for err in errors {
            warn!("[election] {} remount error: {err}", m.target);
        }
    }
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
