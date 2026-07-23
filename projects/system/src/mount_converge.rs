//! Per-host mount convergence loop — the native-mount lifecycle owner that
//! replaces autofs.
//!
//! Each tick this host reads the replicated desired state (its `mounts`
//! placements joined to their `shares`) and makes local reality match: a missing
//! mount is mounted, a stale/unreachable one is remounted onto the next live
//! ordered source, a removed placement is unmounted. Source election, health
//! probing, and the mount itself are all orca's — there is no automounter.
//!
//! The mount/unmount execution goes through the existing root/`sudo` privilege
//! boundary as [`PrivilegedOp::Mount`] / [`PrivilegedOp::Unmount`] — the
//! unprivileged daemon plans, the root helper acts.
//!
//! The decision core ([`plan`]) is pure and unit-tested; the async wrapper only
//! supplies it observed health and executes the actions it returns.

use crate::autofs::{self, PrivilegedOp, run_privileged};
use crate::mount_exec::MountReq;
use crate::{host_identity, mounts, periodic, shares};
use plugin_toolkit::storage::{Health, probe_source};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Seconds between convergence ticks.
pub const INTERVAL_SECS: u64 = 30;
/// Per-target liveness-probe timeout — a live NFS `stat` answers in ms.
pub const PROBE_TIMEOUT_SECS: u64 = 5;
/// Consecutive stale probes before a mounted target is remounted. The blip
/// filter: a single stale probe is usually a briefly-slow server, not a dead
/// one. A *missing* mount is not gated — it is mounted immediately.
pub const CONFIRM_TICKS: u32 = 2;

/// A desired mount for THIS host: a placement joined to its share, with the
/// share's ordered sources and pre-rendered options resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredMount {
    pub target: String,
    pub fstype: String,
    /// Ordered `host:/export` sources, primary first — election picks the first
    /// live one at mount time.
    pub sources: Vec<String>,
    /// The backend-rendered `mount -o` option string (opaque to core).
    pub options: String,
}

/// One convergence action for the privileged applier to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Mount `target` — the async layer elects a live source from `sources`.
    Mount { target: String },
    /// Release `target` (`umount -lf`): stale-and-remounting, or a removed
    /// placement.
    Unmount { target: String },
}

/// Resolve the mount placements assigned to `this_host`, each joined to its
/// share. A placement whose share is missing or is disabled is skipped (it can't
/// be materialized). Shares are keyed by their uuidv7 `id`, which the placement
/// references via `share_id`.
pub fn desired_for_host(this_host: &str) -> anyhow::Result<Vec<DesiredMount>> {
    let by_id: HashMap<String, shares::Share> = shares::endpoint_db::list()?
        .into_iter()
        .filter(|s| s.enabled)
        .map(|s| (s.id.clone(), s))
        .collect();

    let mut out = Vec::new();
    for m in mounts::endpoint_db::list()? {
        if !m.enabled || m.host != this_host {
            continue;
        }
        let Some(share) = by_id.get(&m.share_id) else {
            continue;
        };
        let sources: Vec<String> = serde_json::from_str(&share.sources).unwrap_or_default();
        if sources.is_empty() {
            continue;
        }
        out.push(DesiredMount {
            target: m.target,
            fstype: share.fstype.clone(),
            sources,
            options: share.options_rendered.clone(),
        });
    }
    Ok(out)
}

/// Decide the convergence actions. Pure: given the desired mounts for this host,
/// the set of targets currently mounted at all, and the subset that are mounted
/// **and healthy**, return the mount/unmount actions that make reality match.
///
/// - desired, not mounted        → Mount
/// - desired, mounted but stale   → Unmount then Mount (remount, fail forward)
/// - desired, mounted and healthy → nothing
/// - mounted but no longer desired → Unmount (removed placement)
///
/// Ordering matters: the remount Unmount precedes its Mount, and stray-target
/// Unmounts come last, so the applier can run the vector top-to-bottom.
pub fn plan(
    desired: &[DesiredMount],
    mounted_any: &HashSet<String>,
    mounted_healthy: &HashSet<String>,
) -> Vec<Action> {
    let desired_targets: HashSet<&str> = desired.iter().map(|d| d.target.as_str()).collect();
    let mut actions = Vec::new();

    for d in desired {
        let mounted = mounted_any.contains(&d.target);
        let healthy = mounted_healthy.contains(&d.target);
        if !mounted {
            actions.push(Action::Mount {
                target: d.target.clone(),
            });
        } else if !healthy {
            // Stale: release then remount so election can fail forward onto a
            // live source instead of leaving the wedged superblock.
            actions.push(Action::Unmount {
                target: d.target.clone(),
            });
            actions.push(Action::Mount {
                target: d.target.clone(),
            });
        }
    }

    // Anything mounted that is no longer a desired placement is released.
    for t in mounted_any {
        if !desired_targets.contains(t.as_str()) {
            actions.push(Action::Unmount { target: t.clone() });
        }
    }
    actions
}

/// Build the [`MountReq`] for a desired target from an elected live `source`.
pub fn mount_req(d: &DesiredMount, source: &str) -> MountReq {
    MountReq {
        source: source.to_string(),
        target: d.target.clone(),
        fstype: d.fstype.clone(),
        options: d.options.clone(),
    }
}

/// Elect the first live source from `sources` (probed in priority order), or
/// `None` when every ordered source is down. orca owns source election — one
/// live source is chosen per mount attempt, primary-first so a recovered primary
/// always wins the next tick (fail-back for free). The TCP probe is sync, so it
/// runs on the blocking pool.
async fn elect_source(sources: &[String], fstype: &str, timeout: Duration) -> Option<String> {
    for s in sources {
        let (src, fst) = (s.clone(), fstype.to_string());
        let live = tokio::task::spawn_blocking(move || probe_source(&src, &fst, timeout))
            .await
            .unwrap_or(false);
        if live {
            return Some(s.clone());
        }
    }
    None
}

/// Spawn the per-host convergence loop. Returns the periodic handle the daemon
/// leaks for the process lifetime (scheduler convention).
pub fn spawn() -> JoinHandle<()> {
    // Per-target consecutive-stale counters (the confirm-ticks blip filter),
    // shared across ticks.
    let counters = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
    periodic::spawn(
        periodic::PeriodicSpec {
            name: "storage.converge.run",
            initial_delay: Duration::from_secs(12),
            interval: Duration::from_secs(INTERVAL_SECS),
        },
        periodic::boxed(move || {
            let counters = counters.clone();
            async move { tick(&counters).await }
        }),
    )
}

/// One convergence pass for this host: resolve desired placements, probe each,
/// apply the confirm-ticks blip filter to staleness, plan, and execute the
/// resulting mounts/remounts through the privileged applier. Only ever touches
/// targets that are desired placements — never an NFS mount orca did not declare
/// (so it is safe to run alongside the legacy autofs path during migration).
async fn tick(counters: &Mutex<HashMap<String, u32>>) -> anyhow::Result<()> {
    let this_host = host_identity::machine_id();
    let desired = desired_for_host(this_host)?;
    if desired.is_empty() {
        counters.lock().expect("converge counters poisoned").clear();
        return Ok(());
    }
    let timeout = Duration::from_secs(PROBE_TIMEOUT_SECS);

    // Probe each desired target: mounted+Ok, mounted+stale, or missing.
    let mut mounted_any: HashSet<String> = HashSet::new();
    let mut healthy: HashSet<String> = HashSet::new();
    let mut stale_now: HashSet<String> = HashSet::new();
    for d in &desired {
        match autofs::probe(&d.target, timeout).await {
            Health::Ok => {
                mounted_any.insert(d.target.clone());
                healthy.insert(d.target.clone());
            }
            Health::Missing => {} // not mounted → plan mounts it (not gated)
            Health::Stale | Health::Timeout | Health::Error => {
                mounted_any.insert(d.target.clone());
                stale_now.insert(d.target.clone());
            }
        }
    }

    // Confirm-ticks: advance per-target stale streaks; only a target stale for
    // CONFIRM_TICKS consecutive ticks is remounted. A stale-but-unconfirmed
    // target is kept in `healthy` so `plan` leaves it alone this tick.
    let confirmed_stale = {
        let mut c = counters.lock().expect("converge counters poisoned");
        c.retain(|t, _| desired.iter().any(|d| &d.target == t));
        let mut confirmed = HashSet::new();
        for d in &desired {
            if stale_now.contains(&d.target) {
                let n = c.entry(d.target.clone()).or_insert(0);
                *n += 1;
                if *n >= CONFIRM_TICKS {
                    confirmed.insert(d.target.clone());
                    *n = 0; // reset so a still-down mount re-confirms
                }
            } else {
                c.remove(&d.target);
            }
        }
        confirmed
    };
    for d in &desired {
        if stale_now.contains(&d.target) && !confirmed_stale.contains(&d.target) {
            healthy.insert(d.target.clone()); // ride out the blip
        }
    }

    let actions = plan(&desired, &mounted_any, &healthy);
    if actions.is_empty() {
        return Ok(());
    }

    // Split: unmounts run first (a stale target's release precedes its remount),
    // then mounts, each with a freshly-elected live source.
    let by_target: HashMap<&str, &DesiredMount> =
        desired.iter().map(|d| (d.target.as_str(), d)).collect();
    let mut unmounts: Vec<String> = Vec::new();
    let mut reqs: Vec<MountReq> = Vec::new();
    for a in &actions {
        match a {
            Action::Unmount { target } => unmounts.push(target.clone()),
            Action::Mount { target } => {
                let Some(d) = by_target.get(target.as_str()) else {
                    continue;
                };
                match elect_source(&d.sources, &d.fstype, timeout).await {
                    Some(src) => reqs.push(mount_req(d, &src)),
                    None => warn!(
                        "[converge] {} has NO live source ({} ordered sources down); \
                         leaving unmounted",
                        target,
                        d.sources.len()
                    ),
                }
            }
        }
    }

    if !unmounts.is_empty() {
        let r = run_privileged(&PrivilegedOp::Unmount {
            targets: unmounts.clone(),
        })
        .await;
        for e in &r.errors {
            warn!("[converge] unmount error: {e}");
        }
        for t in &unmounts {
            info!("[converge] released {t} (remounting)");
        }
    }
    if !reqs.is_empty() {
        let r = run_privileged(&PrivilegedOp::Mount { mounts: reqs }).await;
        for t in &r.changed {
            info!("[converge] mounted {t}");
        }
        for e in &r.errors {
            warn!("[converge] mount error: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(target: &str) -> DesiredMount {
        DesiredMount {
            target: target.to_string(),
            fstype: "nfs4".to_string(),
            sources: vec!["10.0.0.1:/e".to_string(), "10.0.0.2:/e".to_string()],
            options: "vers=4.2,soft".to_string(),
        }
    }
    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn missing_desired_is_mounted() {
        let out = plan(&[d("/mnt/data")], &set(&[]), &set(&[]));
        assert_eq!(
            out,
            vec![Action::Mount {
                target: "/mnt/data".into()
            }]
        );
    }

    #[test]
    fn healthy_desired_is_left_alone() {
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data"]),
            &set(&["/mnt/data"]),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn stale_desired_is_unmounted_then_remounted_in_order() {
        let out = plan(&[d("/mnt/data")], &set(&["/mnt/data"]), &set(&[]));
        assert_eq!(
            out,
            vec![
                Action::Unmount {
                    target: "/mnt/data".into()
                },
                Action::Mount {
                    target: "/mnt/data".into()
                },
            ]
        );
    }

    #[test]
    fn undesired_mount_is_released() {
        // /mnt/old is mounted but no longer a placement → unmount.
        let out = plan(
            &[d("/mnt/data")],
            &set(&["/mnt/data", "/mnt/old"]),
            &set(&["/mnt/data"]),
        );
        assert_eq!(
            out,
            vec![Action::Unmount {
                target: "/mnt/old".into()
            }]
        );
    }

    #[test]
    fn mount_req_uses_elected_source_and_rendered_options() {
        let req = mount_req(&d("/mnt/data"), "10.0.0.2:/e");
        assert_eq!(req.source, "10.0.0.2:/e");
        assert_eq!(req.target, "/mnt/data");
        assert_eq!(req.fstype, "nfs4");
        assert_eq!(req.options, "vers=4.2,soft");
    }
}
